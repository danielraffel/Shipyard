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
shipyard queue-observe     # read-only GitHub queue deltas with adaptive backoff
shipyard wait pr 151 --state green  # wait on release / PR / run conditions
shipyard auto-merge <pr>   # cron-friendly one-shot merge-on-green
shipyard merge-queue status  # inspect the local queue-mutation hold
shipyard rescue <pr>       # cancel + redispatch every stuck queued run on a PR
shipyard runner watch --kill-hung-workers  # daemon-mode prevent + auto-kill hung Workers
shipyard update            # self-update the CLI (or `--check` to peek)
shipyard doctor --rate-limit  # inspect REST + GraphQL buckets separately
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
- **Native merge-queue handoff.** On queue-governed branches Shipyard
  validates the exact head, admits that exact SHA, and lets GitHub own the
  merge. Configure one fleet authority with
  `merge_queue.mutation_machine`, pause it instantly with
  `shipyard merge-queue hold --reason "..."`, and audit every attempted
  queue write in machine-global state. Other machines fail before GitHub is
  contacted.
- **22 ecosystem detectors.** `shipyard init` recognises CMake, Swift,
  Xcode, Rust, Go, Node (pnpm/bun/yarn/npm), Python (uv/poetry/pip),
  Gradle, Maven, .NET, Flutter, Dart, Deno, Ruby, Elixir, PHP.
- **Self-hosted runner watchdog.** `shipyard runner status` /
  `cleanup --fix` / `watch --fix` / `watch --kill-hung-workers` detect
  and auto-recover the stuck-runner failure mode (orphaned busy state,
  hung worker, stale queued runs). `runner kill --pid <pid>` is the
  explicit one-shot equivalent. See [docs/runner-watchdog.md](docs/runner-watchdog.md).
- **One-shot PR rescue.** `shipyard rescue <pr>` cancels and
  redispatches every stuck queued workflow run on a PR onto
  `github-hosted` (or any provider via `--to`). `--rerun-failed`
  also re-arms watchdog-cancelled runs; `--all-stuck` is the
  repo-wide variant. Pairs with the watchdog to form a complete
  prevent → recover toolkit.
- **In-tool self-update.** `shipyard update` is the discoverable
  upgrade path (no more curl-pipe to remember); `--check` reports
  installed-vs-available, `--to v0.55.0` pins a specific tag for
  rollback.
- **Graceful GraphQL rate-limit degradation.** `shipyard auto-merge`
  and `shipyard wait pr` fall back to REST automatically when
  GraphQL exhausts (separate 5000/hr bucket). `shipyard doctor
  --rate-limit` shows both buckets so you can see which one is hot.
- **Optional tartci VM integration.** Projects with Apple Silicon VM fleets can
  keep local VM images, caches, and per-host capacity in
  [tartci](https://github.com/danielraffel/tartci), then let Shipyard resolve
  the active profile into one concrete GitHub runner selector before each
  dispatch.
- **Agent-readable runner metrics.** `shipyard metrics` records local command
  timings, imports GitHub Actions jobs, and imports optional tartci VM timing
  exports into a small SQLite store. Agents can ask for summaries, drift
  findings, and placement advice without requiring tartci or any observability
  service.

## Installation

### Claude Code (recommended)

Two commands to register the marketplace and install the plugin:

```bash
claude plugin marketplace add danielraffel/Shipyard
claude plugin install shipyard
```

Then set up your project:

```
/shipyard:init
```

The plugin uses the CLI under the hood. On first session start it
auto-installs the binary if it can't find `shipyard` on PATH — and
skips the install if it can. If you've already installed the CLI
(via `install.sh` or a project pinner like pulp's
`tools/install-shipyard.sh`), make sure its bin directory is on
PATH before you install the plugin; that way the plugin respects
your existing pin instead of installing its own copy alongside it.

Plugin + CLI are independently versioned; the plugin's version
covers slash commands / skills / hooks, while the CLI's version
covers the binary. It's safe to have both.

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
- **Cloud builds** run on managed infrastructure (GitHub Actions by default,
  Namespace where available) for neutral or on-demand capacity.

`shipyard run` delivers the exact commit to each machine, runs your build
and test commands, and reports what passed. `shipyard ship` does the same,
then opens a PR and merges when every required platform is green.

For a multi-Mac fleet, store a stable tag on each host with
`shipyard runner tag --set <studio|m1|m5>` and select exactly one queue
writer in the trusted machine-global `config.toml` reported by
`shipyard paths`:

```toml
[merge_queue]
mutation_machine = "studio"
```

Validation may run anywhere. Only the selected machine can enqueue, disable
auto-merge, or dequeue through Shipyard, and queue writes for one repo/base
are serialized across local processes. During an incident,
`shipyard merge-queue hold --reason "incident"` blocks before GitHub contact;
`shipyard merge-queue resume` removes the hold while retaining the machine
authority check.

Shipyard is not a [CI service](https://en.wikipedia.org/wiki/Continuous_integration),
not a [build system](https://en.wikipedia.org/wiki/Build_automation),
not a [workflow engine](https://en.wikipedia.org/wiki/Workflow_engine).
It calls your build commands and cares about one thing: did they pass?

## Documentation

- [Queue observer](docs/queue-observer.md) — one-query queue snapshots,
  persisted state hashes, delta-only output, replay, and adaptive backoff.
- [Examples & Scenarios](docs/examples.md) — real-world setups for Xcode,
  CMake, Swift, Tauri, etc.
- [Targets & Fallback Chains](docs/targets.md) — how local/SSH/cloud
  targets work and how to chain them.
- [Agent Integration](docs/agent-integration.md) — Claude Code / Codex
  setup, merge strategies.
- [Security & Governance](docs/governance.md) — `solo` vs `multi`
  profiles, branch protection, tag protection.
- [Profiles & Configuration](docs/profiles.md) — switch between local /
  cloud / full setups with one command, plus repo-owned CI routing profiles
  and optional tartci-backed local VM routing.
- [CLI Reference](docs/cli-reference.md#runner-metrics) — record/import/query
  runner performance metrics for agent monitoring.
- [Manual CLI Workflows](docs/workflows.md) — debugging failed runs,
  managing the queue, partial reruns.
- [Resuming an interrupted ship](docs/ship-resume.md) — how `shipyard ship`
  recovers across closed laptops and restarted sessions.
- [Release automation](RELEASING.md) — `shipyard release-bot setup`,
  `doctor --release-chain`, and the PAT + secret setup for the auto-
  release tag → binaries chain.
- [Rust release and rollback](docs/cutover.md) — post-cutover release
  validation, signed macOS packaging, webhook/Funnel validation, GUI and
  consumer notes, and rollback steps.
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
| [gh](https://github.com/cli/cli) | Yes (for PRs) | GitHub integration[^gh-scope] | `brew install gh` |
| `ssh` | For remote targets | Connect to VMs | Pre-installed on macOS / [Ubuntu](https://ubuntu.com/server/docs/how-to/security/openssh-server/) / [Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse?tabs=gui&pivots=windows-11) |
| [nsc](https://namespace.so/docs/reference/cli/installation) | Optional | Namespace runner visibility when your account has access | `brew install namespace-cli` |
| [UTM](https://mac.getutm.app) / [Parallels](https://www.parallels.com/products/desktop/) | For VM fallback | Auto-boot VMs | `brew install --cask utm` |

`shipyard doctor` checks all of this and tells you what's missing.

[^gh-scope]: `gh` needs the **`workflow`** scope (classic PAT) or **Actions: Read and write** (fine-grained) for `shipyard cloud retarget`, `cloud handoff`, and any command that cancels + re-dispatches workflow runs. Quick fix: `gh auth refresh -h github.com -s workflow`. Full setup in [docs/install.md § First-run auth](docs/install.md#first-run-auth).

## This repo uses Shipyard

Shipyard validates and ships itself. The config is in
[`.shipyard/config.toml`](.shipyard/config.toml). The CI workflow at
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs tests on
macOS, Linux, and Windows on every push. The release workflow at
[`.github/workflows/release.yml`](.github/workflows/release.yml) builds
Linux, Windows, and macOS ARM64 release candidates when a version is
tagged; the macOS DMG is signed/notarized and published through the
release runbook.

## FAQ

### Where does each install method put the `shipyard` binary?

All three user-facing install paths write the binary to the same
place:

| Method | Target |
|---|---|
| `curl … install.sh` (manual) | `~/.local/bin/shipyard` |
| Claude Code plugin (auto-installs via `SessionStart` hook if needed) | `~/.local/bin/shipyard` |
| Codex one-liner (same `install.sh`) | `~/.local/bin/shipyard` |

`~/.local/bin` is the canonical location. Make sure it's on your
`PATH` and every install method reaches the same binary. `sy` is a
symlink that resolves to the same `shipyard` binary.

Contributors building from source can run
`cargo build --release --locked` for an isolated checkout build, or
intentionally copy `target/release/shipyard` to `~/.local/bin/shipyard`
when they want the source build to become the system install. See
[`docs/install.md`](docs/install.md).

Project pinners that want a specific version should use
`SHIPYARD_VERSION="v0.22.1" bash install.sh` — it lands at the same
`~/.local/bin/shipyard`, no private toolchain dir required.

### Can I install the Claude Code plugin after installing the CLI via the Codex one-liner?

Yes, and you're meant to. The plugin deliberately defers to an
existing CLI install: on first session its `check-cli.sh` hook runs
`command -v shipyard` before doing anything else. If it finds a
binary on `PATH` it skips the auto-install entirely. If it doesn't,
it runs the same `install.sh` you'd have run by hand and lands at
`~/.local/bin/shipyard`.

Order doesn't matter. CLI first, then plugin → plugin detects the
CLI, no duplicate. Plugin first, then CLI → the auto-installer did
the work already. Both lead to one binary at the canonical location.

### Will installing a newer shipyard via `install.sh` clobber a plugin-installed one (or vice versa)?

Yes, by design. Both land at `~/.local/bin/shipyard` and the later
install wins. The plugin's `SessionStart` hook doesn't re-install
when it sees any `shipyard` on `PATH`, so a fresh manual install
sticks until you explicitly run `install.sh` again. To check which
version you're on at any moment: `shipyard --version`.

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

# 2. Uninstall the CLI binary (install.sh writes here regardless of source)
rm -f ~/.local/bin/shipyard ~/.local/bin/sy

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

Live mode requires the `shipyard` CLI to be installed on the Mac running the app. Install it with `curl -fsSL https://generouscorp.com/Shipyard/install.sh | sh`. If you don't want live mode, you can ignore this — the app will keep working in polling mode.

### Will pushing without `shipyard ship` break anything I've already shipped?

No. A branch force-push or one-off commit on a tracked PR leaves the existing ship-state entry as-is (still scoped to the old SHA) until you explicitly re-ship or archive it. The app may show stale evidence for that PR until then. [Issue #128](https://github.com/danielraffel/Shipyard/issues/128) tracks improving this with passive observer mode.

### Can Shipyard use a larger GitHub API quota?

Yes. Shipyard can authenticate its GitHub calls with a GitHub App installation
token instead of your normal `gh` user token. For non-Enterprise GitHub App
installations, GitHub starts at 5,000 REST requests/hour and scales by
repository count after 20 repositories, up to 12,500 requests/hour. In practice,
an installation with access to 170+ repositories reaches the cap. GitHub Pro is
not the important factor; using an installation access token is.

See [`docs/github-app-quota.md`](docs/github-app-quota.md) for the setup fields,
permissions, Shipyard config, and quota validation commands.

## Learn more

- [Blog post: Shipyard is a cross-platform CI orchestration layer](https://danielraffel.me/2026/04/09/shipyard-is-a-cross-platform-ci-orchestration-layer-that-coordinates-validation-for-ai-agents-working-across-parallel-worktrees/)
- [Pulp](https://github.com/danielraffel/pulp) — the audio plugin
  Shipyard was extracted from, and the first project to adopt it.
- [Shipyard MenuBar for macOS](https://github.com/danielraffel/shipyard-macos-gui) - Shipyard itself runs in the terminal, and that's still the preferred way to drive it. This app is a quick glance at what's happening without dropping into a shell. It's a lightweight menu bar app for quickly viewing and managing CI without using the shell. See per-PR, per-platform status at a glance and jump directly to runs, PRs, or logs. It also lets you retarget jobs, add lanes to in-flight PRs, and access diagnostics (shipyard doctor) in one place.
