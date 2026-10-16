# Shipyard

[![CI](https://github.com/danielraffel/Shipyard/actions/workflows/ci.yml/badge.svg)](https://github.com/danielraffel/Shipyard/actions/workflows/ci.yml)

Shipyard and [TartCI](https://github.com/danielraffel/tartci) solve different
parts of CI:

- **Shipyard coordinates the work.** It sends an exact commit to the targets
  you choose, records the results, watches pull requests, and hands a verified
  head to GitHub for landing.
- **TartCI runs the work in disposable VMs.** It turns Apple Silicon Macs into
  GitHub Actions capacity by booting clean macOS, Linux, or Windows VMs for
  jobs and removing them afterward.

Use Shipyard without TartCI for local, SSH, or hosted checks. Add TartCI when
those checks need isolated Mac VM capacity. GitHub remains the authority for
pull requests and merge order, and your existing build system remains the
authority for how code is built and tested.

## Who it's for

Use Shipyard if you:

- work across several worktrees or machines and need one view of validation;
- want a pull request to land only after the exact head passes every required
  platform;
- run local Macs or private VMs alongside GitHub-hosted runners;
- want unattended monitoring that records a clear reason when it must stop.

Shipyard is useful for people and coding tools. It does not choose what your
product should do or edit code on its own.

## How Shipyard and TartCI work together

```text
person or coding tool
        |
        v
Shipyard chooses targets and records exact-head evidence
        |
        +--> local Mac, SSH host, or GitHub-hosted job
        |
        +--> TartCI -> disposable macOS/Linux/Windows VM
        |
        v
Shipyard reports the evidence; GitHub owns merge order
```

[TartCI](https://github.com/danielraffel/tartci) is the execution layer in
this pairing. It owns the VM images, host budgets, caches, and GitHub runner
lifecycle. 

Shipyard is the coordination layer: it selects the configured TartCI target, waits for the job evidence for the exact commit, and uses that evidence in the pull request workflow. Shipyard does not manage TartCI's VM
images or host budgets.

The boundaries:

- **Shipyard** coordinates commits, targets, evidence, pull requests, review,
  queue handoff, and recovery.
- **TartCI** supplies isolated VM execution capacity.
- **GitHub** owns pull requests, checks, runner assignment, and merge order.

## Quick start

Install the CLI:

```bash
curl -fsSL https://generouscorp.com/Shipyard/install.sh | sh
```

Then initialize a project and run its configured checks:

```bash
cd my-project
shipyard init
shipyard run
shipyard status
```

When the checks are green, `shipyard ship` can open the pull request and follow
it through the configured landing path:

```bash
shipyard ship
```

`shipyard init` writes a starting project configuration. It does not enroll
machines or change GitHub settings. Review the generated targets before using
unattended commands.

For Claude Code, install the plugin separately:

```bash
claude plugin marketplace add danielraffel/Shipyard
claude plugin install shipyard
```

The plugin uses the same CLI. If `shipyard` is already on `PATH`, it uses that
installation.

## What it does

- **Runs the exact commit.** Each target receives one known commit, and the
  result is recorded against that SHA.
- **Coordinates several targets.** Run local, SSH, cloud, and TartCI-backed
  checks from one project configuration.
- **Protects the landing path.** `shipyard ship` refuses to land a change
  without the required exact-head evidence. Optional `ghapp` guards require a
  signed reviewer verdict before arming, enqueueing, or merging.
- **Works with GitHub's merge queue.** Shipyard validates the head, hands the
  exact SHA to GitHub, and records the queue action.
- **Explains failures.** `shipyard diagnose` and `shipyard cause` reduce a red
  job to its failing step and a few useful evidence lines.
- **Recovers supported stuck work.** Runner watchdogs, PR rescue, queue
  observation, and bounded retries handle routine failures without guessing.
- **Keeps state across restarts.** A stopped terminal or daemon does not erase
  the known commit, checks, handoff, or reason for waiting.
- **Supports read-only inspection.** `shipyard inspect --profile` shows what a
  repository builds, pins, runs on, and tests, with the source of each fact.

Read the detailed behavior in the [CLI reference](docs/cli-reference.md),
[landing model](docs/landing-model.md), and [governance guide](docs/governance.md).

## Common setups

### One Mac

Run checks locally and use GitHub for the pull request:

```bash
shipyard init
shipyard run
shipyard pr
```

### Mac fleet with TartCI

TartCI owns the Apple Silicon VM fleet. Shipyard points a project target at
that capacity and combines its result with local, SSH, or hosted checks. See
[targets and fallback chains](docs/targets.md) and the
[TartCI documentation](https://github.com/danielraffel/tartci).

### SSH hosts or private VMs

Declare an SSH target and its build commands. Shipyard copies the exact
commit, runs the commands, and records the result. The target can be a Mac,
Linux VM, Windows host, or an independently managed Proxmox machine.

### GitHub-hosted validation

Use hosted Actions when you need neutral or on-demand capacity. Shipyard can
watch the jobs, diagnose failures, and keep the result tied to the pull
request's exact head.

See [examples and scenarios](docs/examples.md) for complete configurations.

## What it does not do

- It does not replace GitHub's pull request or merge-queue authority.
- It does not replace CMake, Xcode, Cargo, npm, or another build system.
- It does not provide VM images or host capacity; TartCI does that for its
  supported Apple Silicon setup.
- It does not silently edit code, change a product decision, or guess when
  evidence is missing.
- It does not require a daemon for foreground commands. Polling works without
  live webhooks; the daemon adds faster updates and reconciliation.

## About the name

This project is independent of Shipyard CI and other companies or products
using the Shipyard name.

## Documentation

Start here:

- [Installation](docs/install.md): binaries, source builds, and first-run auth.
- [Examples](docs/examples.md): common project and target configurations.
- [Targets](docs/targets.md): local, SSH, cloud, and fallback targets.
- [CLI reference](docs/cli-reference.md): every command and option.

For specific workflows:

- [Agent integration](docs/agent-integration.md): Claude Code, Codex, and
  handoff patterns.
- [Landing model](docs/landing-model.md): exact-head evidence and merge-queue
  behavior.
- [Governance](docs/governance.md): branch protection, tags, and `solo` or
  `multi` profiles.
- [PR watch](docs/pr-watch.md): stuck pull requests and change-driven events.
- [Runner watchdog](docs/runner-watchdog.md): stale runners and recovery.
- [Cause records](docs/cause-records.md): named causes for red checks.
- [Workflows](docs/workflows.md): manual recovery and partial reruns.
- [Ship resume](docs/ship-resume.md): continuing after a terminal or daemon
  restart.
- [Terminal adapters](docs/terminal-adapters.md): terminal and provider
  boundaries.
- [Release automation](RELEASING.md): release setup and rollback.

## Requirements

You need only the tools used by your setup:

| Tool | Needed for |
| --- | --- |
| [Git](https://git-scm.com/) | Every project |
| [GitHub CLI](https://cli.github.com/) | Pull requests and GitHub Actions |
| [SSH](https://www.openssh.com/) | Remote targets |
| [Namespace CLI](https://namespace.so/docs/reference/cli/installation) | Optional Namespace runner visibility |
| [TartCI](https://github.com/danielraffel/tartci) | Optional disposable Apple Silicon VMs |

Run `shipyard doctor` to check the tools and credentials for the current
configuration.

## This repository uses Shipyard

Shipyard validates itself on macOS, Linux, and Windows. The workflows and
project configuration are in [`.shipyard/config.toml`](https://github.com/danielraffel/Shipyard/blob/main/.shipyard/config.toml)
and [`.github/workflows/`](https://github.com/danielraffel/Shipyard/tree/main/.github/workflows). Release details are in
[RELEASING.md](RELEASING.md).

## Contributing

Build and test from a checkout:

```bash
cargo build --release --locked
cargo test --locked
```

Run the repository's configured checks before opening a pull request.

## License

See [LICENSE](LICENSE).
