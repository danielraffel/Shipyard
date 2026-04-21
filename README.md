# Shipyard

**Cross-platform CI for AI agents.** Validates the exact commit on every
machine you configure — local Mac, SSH-reachable VMs, cloud runners — and
merges only when everything is green.

```bash
curl -fsSL https://generouscorp.com/Shipyard/install.sh | sh
cd my-project
shipyard init              # detects your project, probes your machines
shipyard run               # validates on every platform you configured
shipyard ship              # validate, open PR, merge on green
shipyard watch             # live-tail an in-flight ship
shipyard auto-merge <pr>   # cron-friendly one-shot merge-on-green
shipyard release-bot setup # guided RELEASE_BOT_TOKEN setup
shipyard cloud retarget    # switch one target's runner mid-flight
shipyard cloud add-lane    # append a new lane to an in-flight PR
shipyard changelog init    # opt in to post-release CHANGELOG auto-sync
```

## Highlights

- **Designed for AI agents.** Claude Code and Codex one-command setup;
  agents validate and merge without you handing them notarization keys
  or publishing accounts.
- **Evidence-based merge gate.** `shipyard ship` refuses to merge unless
  every required platform has passing evidence **for the exact HEAD SHA** —
  not the most-recent run, not the branch tip, the SHA.
- **Parallel-agent-aware queue.** Multiple agents in multiple worktrees
  share one machine-global queue with priorities, FIFO scheduling, and
  automatic deduplication.
- **Declarative security & governance.** One TOML line picks a profile
  (`solo` or `multi`); one CLI command makes GitHub branch protection,
  tag protection, and workflow token permissions match.
- **22 ecosystem detectors.** `shipyard init` recognises CMake, Swift,
  Xcode, Rust, Go, Node (pnpm/bun/yarn/npm), Python (uv/poetry/pip),
  Gradle, Maven, .NET, Flutter, Dart, Deno, Ruby, Elixir, PHP.

## Installation

### Claude Code (recommended)

**Step 1:** Add the Shipyard marketplace to `~/.claude/settings.json`:

```json
{
  "extraKnownMarketplaces": {
    "shipyard": {
      "source": {
        "source": "github",
        "repo": "danielraffel/Shipyard"
      }
    }
  }
}
```

**Step 2:** Install the plugin:

```
/plugin install shipyard@shipyard
```

**Step 3:** Set up your project:

```
/shipyard:init
```

The plugin uses the CLI under the hood. If the `shipyard` binary isn't
installed, the plugin will offer to install it for you.

### Codex / CLI

```bash
curl -fsSL https://generouscorp.com/Shipyard/install.sh | sh
shipyard init
```

Downloads a standalone binary for your platform. No runtime needed. See
[install details](docs/install.md) for binary table and build-from-source.

## How it works

- **Local builds** run on your host machine — fast, no network.
- **Remote builds** run on machines you control via SSH — VMs, containers,
  or hosts on your network.
- **Cloud builds** run on managed infrastructure (Namespace, GitHub
  Actions) for neutral or on-demand capacity.

`shipyard run` delivers the exact commit to each machine, runs your build
and test commands, and reports what passed. `shipyard ship` does the same,
then opens a PR and merges when every required platform is green.

Shipyard is not a [CI service](https://en.wikipedia.org/wiki/Continuous_integration),
not a [build system](https://en.wikipedia.org/wiki/Build_automation),
not a [workflow engine](https://en.wikipedia.org/wiki/Workflow_engine).
It calls your build commands and cares about one thing: did they pass?

## Documentation

- [Examples & Scenarios](docs/examples.md) — real-world setups for Xcode,
  CMake, Swift, Tauri, etc.
- [Targets & Fallback Chains](docs/targets.md) — how local/SSH/cloud
  targets work and how to chain them.
- [Agent Integration](docs/agent-integration.md) — Claude Code / Codex
  setup, merge strategies.
- [Security & Governance](docs/governance.md) — `solo` vs `multi`
  profiles, branch protection, tag protection.
- [Profiles & Configuration](docs/profiles.md) — switch between local /
  cloud / full setups with one command.
- [Manual CLI Workflows](docs/workflows.md) — debugging failed runs,
  managing the queue, partial reruns.
- [Resuming an interrupted ship](docs/ship-resume.md) — how `shipyard ship`
  recovers across closed laptops and restarted sessions.
- [Release automation](RELEASING.md) — `shipyard release-bot setup`,
  `doctor --release-chain`, and the PAT + secret setup for the auto-
  release tag → binaries chain.
- [Mid-flight runner retargeting](docs/cloud-retarget.md) — switch one
  target's runner provider on an open PR without tearing down the
  other targets' jobs.
- [CLI Reference](docs/cli-reference.md) — every command and flag.
- [Install details](docs/install.md) — binaries, build from source,
  optional dependencies.

## Requirements

You don't need everything — just what matches your setup:

| Tool | Required? | What it's for | Install |
|------|-----------|---------------|---------|
| [git](https://github.com/git-guides/install-git) | Yes | Version control | Pre-installed on macOS |
| [gh](https://github.com/cli/cli) | Yes (for PRs) | GitHub integration | `brew install gh` |
| `ssh` | For remote targets | Connect to VMs | Pre-installed on macOS / [Ubuntu](https://ubuntu.com/server/docs/how-to/security/openssh-server/) / [Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse?tabs=gui&pivots=windows-11) |
| [nsc](https://namespace.so/docs/reference/cli/installation) | For [Namespace](https://namespace.so) | Cloud runners | `brew install namespace-cli` |
| [UTM](https://mac.getutm.app) / [Parallels](https://www.parallels.com/products/desktop/) | For VM fallback | Auto-boot VMs | `brew install --cask utm` |

`shipyard doctor` checks all of this and tells you what's missing.

## This repo uses Shipyard

Shipyard validates and ships itself. The config is in
[`.shipyard/config.toml`](.shipyard/config.toml). The CI workflow at
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs tests on
macOS, Linux, and Windows on every push. The release workflow at
[`.github/workflows/release.yml`](.github/workflows/release.yml) builds
binaries on 5 platforms when a version is tagged.

## FAQ

### Do I need to run `shipyard daemon` / enable live mode?

No. The daemon is an optional optimization for realtime webhook updates. Without it, every shipyard command falls back to polling — behavior is identical to earlier versions. `shipyard run`, `ship`, `watch`, `auto-merge`, and the macOS app all work fine without the daemon.

### Does it hurt if I don't enable live mode?

No. You'll still get the same results; they just arrive on a poll cadence (60 s worst case) rather than push-instant. Webhooks aren't registered on your repos unless the daemon is running. No Tailscale Funnel is created if you don't run `shipyard daemon start`.

### I pushed to a repo without running `shipyard ship`. Will it appear in the macOS app?

Depends on whether you've ever shipped from that repo on this machine:

- **Never shipped from that repo before** → nothing appears. The app only tracks repos it knows about via local ship-state.
- **You've shipped at least one PR from that repo before** → pushes show up in the "GitHub Actions" section of the app (polled via `gh run list` for known repos), but not as a tracked PR card. Tracked PR cards only appear for PRs that have ship-state — i.e. PRs you invoked `shipyard ship` or `shipyard pr` on.

If live mode is on, the daemon will deliver webhook events for those pushes too, so the "GitHub Actions" section updates in realtime — but it still won't promote an un-shipped PR into a tracked card.

### How do I turn off live mode?

- **From the macOS app**: Settings → Live updates → **Off**. The app sends a stop command to the daemon, which unregisters webhooks and resets the Tailscale Funnel config. Nothing persists after that.
- **From the CLI**: `shipyard daemon stop` does the same teardown.

### How do I remove everything shipyard installed?

Shipyard doesn't leave much footprint, but here's the complete list:

```bash
# 1. Stop + unregister the daemon (if running)
shipyard daemon stop

# 2. Uninstall the CLI (method depends on how you installed)
pip uninstall shipyard                # if installed via pip
rm -rf ~/.pulp                        # if installed via install.sh

# 3. Remove state directory (ship-state, daemon config, webhook secret)
#    macOS:
rm -rf ~/Library/Application\ Support/shipyard
#    Linux:
rm -rf ~/.local/state/shipyard

# 4. (macOS only) Keychain entry for the webhook secret
security delete-generic-password -s com.danielraffel.shipyard.webhook
```

The macOS menu-bar app (`shipyard-macos-gui`) is separate: drag it out of `/Applications` to uninstall.

### I don't have Tailscale. Is live mode usable?

Not in v1. Tailscale Funnel is the only tunnel backend shipped currently; others (Cloudflare Tunnel, ngrok, user-supplied reverse proxy) are tracked in [issue #126](https://github.com/danielraffel/Shipyard/issues/126). Until those land, live mode requires Tailscale + Funnel. The rest of shipyard (polling path) works fine without either.

### Does shipyard read or store any secrets besides the webhook HMAC?

- The webhook HMAC secret is the only secret shipyard stores — in macOS Keychain or a `600`-perm file on Linux. It's generated locally, only sent to GitHub (as part of the webhook registration), and only used to verify that incoming deliveries actually came from GitHub.
- `gh` auth is read through the `gh` CLI's existing token storage. Shipyard doesn't duplicate or persist it.
- SSH keys for remote targets are whatever's already in your `~/.ssh/`.

### My macOS app says "shipyard CLI not found on PATH"

Live mode requires the `shipyard` CLI to be installed on the Mac running the app. Install it (`curl -fsSL https://install.shipyard.sh | sh` or via `pip install shipyard`). If you don't want live mode, you can ignore this — the app will keep working in polling mode.

### Will pushing without `shipyard ship` break anything I've already shipped?

No. A branch force-push or one-off commit on a tracked PR leaves the existing ship-state entry as-is (still scoped to the old SHA) until you explicitly re-ship or archive it. The app may show stale evidence for that PR until then. [Issue #128](https://github.com/danielraffel/Shipyard/issues/128) tracks improving this with passive observer mode.

## Learn more

- [Blog post: Shipyard is a cross-platform CI orchestration layer](https://danielraffel.me/2026/04/09/shipyard-is-a-cross-platform-ci-orchestration-layer-that-coordinates-validation-for-ai-agents-working-across-parallel-worktrees/)
- [Pulp](https://github.com/danielraffel/pulp) — the audio plugin
  Shipyard was extracted from, and the first project to adopt it.
- [Shipyard MenuBar for macOS](https://github.com/danielraffel/shipyard-macos-gui) - Shipyard itself runs in the terminal, and that's still the preferred way to drive it. This app is a quick glance at what's happening without dropping into a shell. It's a lightweight menu bar app for quickly viewing and managing CI without using the shell. See per-PR, per-platform status at a glance and jump directly to runs, PRs, or logs. It also lets you retarget jobs, add lanes to in-flight PRs, and access diagnostics (shipyard doctor) in one place.
