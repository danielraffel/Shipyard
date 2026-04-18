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

## Learn more

- [Blog post: Shipyard is a cross-platform CI orchestration layer](https://danielraffel.me/2026/04/09/shipyard-is-a-cross-platform-ci-orchestration-layer-that-coordinates-validation-for-ai-agents-working-across-parallel-worktrees/)
- [Pulp](https://github.com/danielraffel/pulp) — the audio plugin
  Shipyard was extracted from, and the first project to adopt it.
