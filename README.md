# Shipyard

**Shipyard handles the waiting so people and coding tools can focus on the
work.** For work you hand it, Shipyard watches tests and code changes, remembers
the exact state, handles supported routine steps automatically, and stops with
a clear reason when a decision or code change is needed.

Under the hood, Shipyard tracks the exact code and workflow state, checks each
configured machine, and moves forward only when the required evidence is
green. GitHub retains merge-order authority; when configured, TartCI supplies
isolated Mac execution capacity.

```bash
curl -fsSL https://generouscorp.com/Shipyard/install.sh | sh
cd my-project
shipyard init              # detects your project, probes your machines
shipyard run               # validates on every platform you configured
shipyard ship              # validate, open PR, merge on green
shipyard watch             # live-tail an in-flight ship
shipyard queue-observe     # read-only GitHub queue deltas with adaptive backoff
shipyard changed-surface-plan --pr 123 --target mac  # fail-closed shadow test plan
shipyard changed-surface-trial-status --repo owner/repo --pr 123 --target mac --head "$HEAD_SHA"  # verify shadow comparison
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

## How it fits

Shipyard sits between the work and the machines that run it:

1. **Work.** A person or coding tool decides what to change and writes the
   code. Shipyard does not invent a product decision or silently edit code.
2. **Coordination.** Shipyard records the exact change, its checks, handoff,
   and allowed next steps. With the optional daemon enabled, verified GitHub
   webhooks provide quick updates and periodic GitHub reconciliation repairs a
   missed update or restart. Routine observation does not need a model; a
   decision, ambiguous failure, or code repair is surfaced for one.
3. **Execution.** GitHub remains the authority for merge order and hosted-job
   assignment. Shipyard can ask configured machines to validate a change.
   [TartCI](https://github.com/danielraffel/tartci) supplies isolated Mac VM
   capacity; SSH targets, including independently managed Proxmox Linux VMs,
   remain separate executors under the same evidence rules.

All three layers are available now when their executors are configured. Durable
continuation within the coordination layer is deliberately opt-in and
policy-gated: it records a specific handoff and refuses when the record, route,
or current code no longer agrees. It is not a promise that an unconfigured
project will automatically recover or merge every PR.

## Common uses

- Check the same code change on local Macs, TartCI Mac VMs, Linux machines or
  Proxmox-hosted VMs over SSH, Windows, and hosted runners.
- Submit a change and, when stewardship is enabled, let Shipyard watch the
  required tests and merge progress until the work finishes or needs help.
- Keep long-running work understandable across terminal, daemon, and machine
  restarts without reconstructing its state from scratch.
- Use the `ghapp` wrapper for scoped GitHub App authentication without putting
  short-lived credentials into unattended command lines.
- Keep terminal delivery and model-provider routing separate. `cmux` is the
  physically implemented terminal adapter today. HerdR endpoint shapes and
  Subrouter provider routes are registered, but HerdR delivery remains
  fail-closed until its own live capability checks pass. See [terminal and
  provider adapters](docs/terminal-adapters.md).

## Highlights

- **Built for bounded unattended delivery.** With an explicit, authorized
  handoff, Shipyard preserves exact state, watches the required systems, and
  takes only its next allowed action. It records uncertainty rather than
  guessing, and it never puts notarization keys or publishing accounts in a
  command line.
- **Evidence-based merge gate.** `shipyard ship` refuses to merge unless
  every required platform has passing evidence **for the exact HEAD SHA** —
  not the most-recent run, not the branch tip, the SHA.
- **Fail-closed test-selection shadowing.** A target can declare mandatory
  baseline smoke plus complete changed-surface families and their reviewed
  build targets. Shipyard authenticates PR/protected-base provenance and emits
  an exact-head receipt while the full build/test path remains authoritative;
  ambiguity always falls back to full.
- **Parallel-work-aware queue.** Multiple worktrees and clients share one
  machine-global queue with priorities, FIFO scheduling, and
  automatic deduplication.
- **Durable stewardship and continuation.** An exact PR/head handoff can be
  persisted in the machine-global work ledger so the trusted daemon, rather
  than a client polling loop, owns bounded monitoring and continuation across
  process restarts. Activation is explicit and default-off; every transition
  is generation-fenced and revalidates the protected route before dispatch.
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
- **Qualified immutable dependency pins.** Opted-in first-party repositories
  can follow the latest qualified Pulp release, while production and frozen
  repositories select explicit stable or fixed identities. Shipyard verifies
  release and build attestations, materializes exact digests, and opens the pin
  PR with GitHub App authority. See
  [Pulp dependency channels](docs/dependency-channels.md).
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
  dispatches fresh replacements for terminal failed/cancelled runs without
  re-arming the originals; `--all-stuck` is the
  repo-wide variant. Pairs with the watchdog to form a complete
  prevent → recover toolkit.
- **Durable cancellation.** `shipyard cancel <job> --reason <why>` records the
  operator reason and terminates an active local or SSH validation process tree
  on its next progress event, including descendant build processes.
- **In-tool self-update.** `shipyard update` is the discoverable
  upgrade path (no curl-pipe to remember); it uses the trusted machine-global
  GitHub auth helper and a tag-matched installer, and `--check` reports
  installed-vs-available, `--to v0.55.0` pins a specific tag for
  rollback. `shipyard runner fleet-update --to vX.Y.Z` plans one governed
  rollout; add `--apply` to publish one immutable, content-addressed auth
  generation and refresh every configured daemon. The public wrapper selector
  changes only after the complete release-matched generation validates, and
  each host returns an exact generation and release receipt.
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

See [exact-head changed-surface selection](docs/changed-surface-selection.md)
for the base-owned schema, receipt fields, and hard-fail/fallback boundary.

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

Repositories may make submitting-session provenance an atomic PR precondition:

```toml
[pr.provenance]
command = ["whence", "--pr", "{pr}", "--auto"]
required = true
```

`shipyard pr` executes this argv directly after the exact PR/head is known and
before any steward receipt or validation dispatch. Supported placeholders are
`{pr}`, `{repo}`, `{head}`, `{branch}`, `{base}`, and `{url}`; the same values
are also passed as `SHIPYARD_PR_*` environment variables. The hook inherits the
agent's cmux/provider environment. A configured hook is required by default and
fails closed, so interrupting the later CI watcher cannot leave a managed PR
without its provenance. Explicit `shipyard ship --pr` recovery never runs the
hook and therefore cannot overwrite the original submitting context.

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
  runner performance metrics for fleet monitoring.
- [Manual CLI Workflows](docs/workflows.md) — debugging failed runs,
  managing the queue, partial reruns.
- [Resuming an interrupted ship](docs/ship-resume.md) — how `shipyard ship`
  recovers across closed laptops and restarted sessions.
- [Launch profiles](docs/launch-profile.md) — protected, generation-bound
  resume and fresh-agent metadata for default-off daemon continuation.
- [Terminal and provider adapters](docs/terminal-adapters.md) — why terminal
  transport is separate from provider routing, including the current cmux
  boundary and fail-closed, physically unproven HerdR delivery status.
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

### Where does each install method put the Shipyard binaries?

All three user-facing install paths write the CLI and its workstream-provider
companion to the same directory:

| Method | Target |
|---|---|
| `curl … install.sh` (manual) | `~/.local/bin/shipyard` and `~/.local/bin/shipyard-workstream-provider` |
| Claude Code plugin (auto-installs via `SessionStart` hook if needed) | same pair |
| Codex one-liner (same `install.sh`) | same pair |

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

Not for foreground CI. Without the daemon, `shipyard run`, `ship`, `watch`,
`auto-merge`, and the macOS app retain their polling fallback. The daemon is
required only when you explicitly enable trusted, subscriber-independent
workstream continuation: that path owns durable monitoring and generation-
fenced wake delivery after the submitting terminal or agent disappears.

### Does it hurt if I don't enable live mode?

Foreground commands still reach the same evidence verdicts; updates arrive on
a poll cadence (60 s worst case) rather than push-instant. Webhooks aren't
registered and no Tailscale Funnel is created unless the daemon is running.
Default-off unattended workstream continuation is different: without its
trusted daemon consumer, no process owns a durable wake after the subscriber
exits.

### I pushed to a repo without running `shipyard ship`. Will it appear in the macOS app?

Depends on whether you've ever shipped from that repo on this machine:

- **Never shipped from that repo before** → nothing appears. The app only tracks repos it knows about via local ship-state.
- **You've shipped at least one PR from that repo before** → pushes show up in the "GitHub Actions" section of the app (polled via `gh run list` for known repos), but not as a tracked PR card. Tracked PR cards only appear for PRs that have ship-state — i.e. PRs you invoked `shipyard ship` or `shipyard pr` on.

If live mode is on, the daemon will deliver webhook events for those pushes too, so the "GitHub Actions" section updates in realtime — but it still won't promote an un-shipped PR into a tracked card.

### How do I turn off live mode?

- **From the macOS app**: Settings → Live updates → **Off**. The app sends a stop command to the daemon, which unregisters webhooks and resets the Tailscale Funnel config. Nothing persists after that.
- **From the CLI**: `shipyard daemon stop` does the same teardown.

Stopping the daemon also stops subscriber-independent workstream wake delivery.
The durable ledger remains on disk, but no new wake is delivered until an
authorized daemon consumer is enabled again.

### How do I remove everything shipyard installed?

Shipyard doesn't leave much footprint, but here's the complete list:

```bash
# 1. Stop + unregister the daemon (if running)
shipyard daemon stop

# 2. Uninstall the public CLI/provider/auth projections and CLI alias.
#    If your host profile uses different paths, remove its exact configured
#    github_cli/github_token_helper paths and the context beside github_cli.
rm -f ~/.local/bin/shipyard ~/.local/bin/shipyard-workstream-provider ~/.local/bin/ghapp ~/.local/bin/ghapp.shipyard-context.json ~/.local/bin/sy
rm -f ~/.config/shipyard/bin/shipyard-github-app-token

# 3. After the daemon and every ghapp reader have stopped, remove immutable
#    fleet auth generations. Never remove this while a wrapper is running.
rm -rf ~/.local/share/shipyard/auth-generations

# 4. Remove state directory (ship-state, daemon config, webhook secret)
#    macOS:
rm -rf ~/Library/Application\ Support/shipyard
#    Linux:
rm -rf ~/.local/state/shipyard

# 5. (macOS only) Keychain entry for the webhook secret
security delete-generic-password -s com.danielraffel.shipyard.webhook
```

The macOS menu-bar app (`shipyard-macos-gui`) is separate: drag it out of `/Applications` to uninstall.

### I don't have Tailscale. Is live mode usable?

Not in v1. Tailscale Funnel is the only tunnel backend shipped currently; others (Cloudflare Tunnel, ngrok, user-supplied reverse proxy) are tracked in [issue #126](https://github.com/danielraffel/Shipyard/issues/126). Until those land, live mode requires Tailscale + Funnel. The rest of shipyard (polling path) works fine without either.

### Does shipyard read or store any secrets besides the webhook HMAC?

- The webhook HMAC is generated locally and stored in macOS Keychain or a
  `600`-permission file on Linux. It is sent only to GitHub during webhook
  registration and verifies incoming deliveries.
- With ambient `gh` authentication, Shipyard reads the CLI's existing token
  storage and does not duplicate it.
- Optional GitHub App authentication uses an operator-provisioned `0600` PEM
  private key and may keep short-lived installation tokens in an owner-only
  `0700` cache directory with `0600` entries. The immutable fleet auth
  generation contains the release-matched helper, wrapper, binaries, manifest,
  and non-secret resolver context; it never embeds a token or private key.
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

Runner inventory is a separate permission surface from workflow runs. `Actions:
Read-only` lets Shipyard inspect workflows, runs, and jobs; it does not authorize
`/repos/{owner}/{repo}/actions/runners`. Fleet admission, runner inventory, and
stale-runner proof need repository `Administration: Read-only` so Shipyard can
observe registered runners and their online/busy state, and defer when that
state cannot be read. Grant `Administration: Read & write` to any Shipyard
credential that must mint or delete runner registrations, including interactive
`shipyard runner register` or `shipyard runner remove` use. Organization
runner-group verification separately needs organization `Self-hosted runners:
Read-only`. These reads are what let Shipyard preserve healthy work and fail
closed instead of treating an unreadable pool as idle.

The App can also give a Shipyard deployment's external policy verifier read-only
visibility into organization runner groups. That permission is separate from
repository `Actions` access and lets the integration verify repository, workflow,
and runner-membership boundaries while Shipyard coordinates heterogeneous local
capacity. This is not currently a built-in `shipyard runner` check. TartCI VMs, a separate Proxmox pool,
and native machines remain distinct execution providers; Shipyard supplies the
shared policy and exact-head view. Approve the installation update and refresh
cached tokens after adding the permission.

## Learn more

- [Blog post: Shipyard is a cross-platform CI orchestration layer](https://danielraffel.me/2026/04/09/shipyard-is-a-cross-platform-ci-orchestration-layer-that-coordinates-validation-for-ai-agents-working-across-parallel-worktrees/)
- [Pulp](https://github.com/Generous-Corp/pulp) — the audio plugin
  Shipyard was extracted from, and the first project to adopt it.
- [Shipyard MenuBar for macOS](https://github.com/danielraffel/shipyard-macos-gui) - Shipyard itself runs in the terminal, and that's still the preferred way to drive it. This app is a quick glance at what's happening without dropping into a shell. It's a lightweight menu bar app for quickly viewing and managing CI without using the shell. See per-PR, per-platform status at a glance and jump directly to runs, PRs, or logs. It also lets you retarget jobs, add lanes to in-flight PRs, and access diagnostics (shipyard doctor) in one place.
