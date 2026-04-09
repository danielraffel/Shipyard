# Shipyard

Cross-platform CI from your machine. Local VMs, SSH hosts, cloud runners —
one config, automatic failover, and structured output for AI agents.

```bash
curl -fsSL https://raw.githubusercontent.com/danielraffel/Shipyard/main/install.sh | sh
cd my-project
shipyard init        # detects your project, probes your machines
shipyard run         # validates on every platform you configured
```

---

## Who This Is For

**You’re running agents in parallel across worktrees — sometimes multi-platform, sometimes local, sometimes cloud.**
You want builds you can trust, without heavyweight CI.

## How It Works

- **Local builds** run directly on your host machine — fast, no network
- **Remote builds** run in separate environments you control — VMs, containers, or machines over SSH (local or on your network; physical location doesn’t matter)
- **Cloud builds** run on managed infrastructure — Namespace, GitHub Actions, etc., for neutral or on-demand capacity

`shipyard run` delivers the exact commit to each machine, runs your build
and test commands, and reports what passed. If a machine is unreachable,
Shipyard can try the next option automatically — boot a VM, or dispatch
to cloud. Or it just reports unreachable and stops. You choose.

`shipyard ship` validates and then creates a PR and merges it.

Shipyard is not a CI service, not a build system, not a workflow engine.
It calls your build commands and cares about one thing: did they pass?

---

## Examples

### Scenario 1: You're building a macOS and iOS app

You have an Xcode project. You want to make sure it builds and tests pass
on your Mac before merging. Both targets run locally — no VMs or cloud needed.

```
$ shipyard init

Detecting project...
  Found: MyApp.xcodeproj (Xcode project)
  Platforms detected: macOS, iOS

What platforms do you want to validate?
  [x] macOS    (local Mac — Xcode 16.2 found)
  [x] iOS      (local simulator — iPhone 16 Pro available)

Writing .shipyard/config.toml... done
```

Now every time you run `shipyard run`, it builds and tests on your Mac:

```
$ shipyard run
  macos   = pass  (local, 1m42s)     ← built and tested on your Mac
  ios-sim = pass  (local, 2m15s)     ← ran on the local iOS simulator
  All green.
```

Both targets say `local` because everything runs on your machine. No network,
no VMs, no cloud accounts needed. This is the simplest Shipyard setup.

---

### Scenario 2: You're building a cross-platform audio plugin

You're using JUCE, Pulp, or another C++ framework. Your plugin needs to
compile and pass tests on macOS, Windows, and Linux — because DAW users are
on all three.

You have UTM running a Windows 11 VM and an Ubuntu VM on your Mac. Shipyard
detects them and uses SSH to send your code to each one:

```
$ shipyard init

Detecting project...
  Found: CMakeLists.txt (CMake C++ project)
  Platforms detected: macOS, Windows, Linux

What platforms do you want to validate?
  [x] macOS    (local Mac)
  [x] Windows  (SSH host "win" — reachable, 23ms)
  [x] Linux    (SSH host "ubuntu" — reachable, 847ms)

Cloud failover: fall back to Namespace when VMs are down? [Y/n]

Writing .shipyard/config.toml... done
```

When you run validation, your Mac builds locally while your VMs build over SSH
— all three in parallel:

```
$ shipyard run
  mac     = pass  (local, 3m12s)     ← built on your Mac
  windows = pass  (ssh, 5m30s)       ← built on your Windows VM via SSH
  ubuntu  = pass  (ssh, 4m18s)       ← built on your Ubuntu VM via SSH
  All green.
```

If your VMs are powered off or unreachable, Shipyard automatically falls back
to Namespace cloud runners — you don't have to do anything:

```
$ shipyard run
  mac     = pass  (local, 3m12s)
  windows → SSH unreachable → dispatching to Namespace...
          = pass  (namespace-failover, 8m45s)    ← cloud runner took over
  ubuntu  = pass  (ssh, 4m18s)
  All green.
```

---

### Scenario 3: You're building a macOS desktop app with agents running in parallel in multiple worktrees

Single platform, single machine. You still get Shipyard's queue (so agents running in parallel in multiple
worktrees don't collide), evidence tracking (so you know what SHA last
passed), and one-command merge:

```
$ shipyard init

Detecting project...
  Found: Package.swift (Swift package)
  Platforms detected: macOS

Writing .shipyard/config.toml... done

$ shipyard run
  macos = pass  (local, 45s)

$ shipyard ship
  Created PR #7. Validated. Merged.
```

If you later need Windows or Linux builds, just add targets — no re-init:

```
$ shipyard targets add ubuntu
  SSH host "ubuntu" — reachable. Added.
```

---

### Scenario 4: You're building a cross-platform Tauri app

Tauri apps ship native Rust binaries on macOS, Windows, and Linux. Shipyard
validates all three in parallel:

```
$ shipyard init

Detecting project...
  Found: Cargo.toml (Rust project)
  Found: package.json (Node.js frontend)
  Found: src-tauri/ (Tauri app detected)
  Platforms detected: macOS, Windows, Linux

Writing .shipyard/config.toml... done

$ shipyard run
  mac     = pass  (local, 2m08s)
  ubuntu  = pass  (ssh, 3m45s)
  windows → SSH unreachable → booting VM "Windows 11"...
          = pass  (utm-fallback, 6m30s)    ← Shipyard booted the VM automatically
  All green.
```

The Windows VM was asleep. Shipyard booted it via UTM, waited for SSH to come
up, ran the build, and reported the result.

---

## How Targets Work

A target is a real machine where your code gets validated. You name them
whatever you want and can have as many as you need:

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

### What happens when a machine is down

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

### What Shipyard checks on setup

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
In the future, `shipyard doctor --fix` will offer to install missing tools
for you.

---

## Agent Integration

Shipyard works with any AI coding agent that can run shell commands. Every
command supports `--json` for structured output — agents parse the result
and decide what to do next.

### The "set it and forget it" flow

The most powerful setup: you tell your agent to work on a task, and it
automatically validates and merges when done. No manual CI checks. No
manual merges. You come back and it's on main.

Here's how it works in practice:

1. You: "Implement the reverb effect and ship it"
2. Agent writes code, commits to a feature branch
3. Agent runs `shipyard ship --json` which:
   - Pushes the branch
   - Creates a PR
   - Validates on all configured platforms (Mac + VMs + cloud)
   - If all green, merges to main automatically
4. You come back, it's merged

This is how Pulp (the project Shipyard was extracted from) operates daily.

### Setting up auto-merge for Claude Code

Add this skill file to your project. Claude will use it whenever it needs
to commit, validate, or merge code:

**`.claude/skills/ci.md`:**

```markdown
---
name: ci
description: Cross-platform CI via Shipyard — validates and ships code
---

## After finishing work

When your work is complete and ready to merge:

1. Commit changes to the current feature branch
2. Run: shipyard ship --json
3. This pushes, creates a PR, validates on all platforms, and merges on green
4. If validation fails, read the logs, fix the issue, and run ship again

## To validate without merging

    shipyard run --json

## To check status

    shipyard status --json

## Rules
- Never push directly to main — always use shipyard ship
- If a target fails, fix and re-run before merging
- All configured platforms must be green before merge
```

### Setting up auto-merge for Codex

Add to your `AGENTS.md`:

```markdown
## CI

After completing work, validate and merge:
- Run `shipyard ship` to push, create a PR, validate, and merge on green
- If validation fails, check `shipyard logs <id> --target <name>` for details
- Never push directly to main
```

### Merging to develop instead of main

If you want agents to merge to a `develop` branch instead (less risky for
shared projects), just change the skill:

```markdown
## After finishing work

    shipyard ship --base develop --json
```

You can even have both flows — agents merge to `develop` automatically, and
you manually promote `develop` to `main` when you're ready:

```bash
git checkout develop
shipyard ship --base main    # validate develop, merge to main
```

### Automated CI hook (optional)

If you want CI to trigger automatically after every push (not just when
shipping), add a hook to `.claude/settings.json`:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Bash",
        "command": "if echo \"$TOOL_INPUT\" | grep -q 'git push'; then shipyard run --json 2>/dev/null || true; fi"
      }
    ]
  }
}
```

---

## Workflow Scenarios

### Scenario: You finished a feature and want to merge

You've been working on a feature branch. Everything looks good. Time to
validate across platforms and merge.

```
$ shipyard run
  mac     = pass  (local, 3m12s)
  ubuntu  = pass  (ssh, 5m30s)
  windows = pass  (ssh, 4m18s)
  All green.

$ shipyard ship
  PR #42 created → Validated → Merged to main
```

Or in one step: `shipyard ship` does the validation and merge together.

### Scenario: CI fails on one platform

You ran validation and Windows failed. You don't want to re-validate
macOS and Linux (they already passed) — just fix and re-run Windows.

```
$ shipyard run
  mac = pass, ubuntu = pass, windows = FAIL

$ shipyard logs sy-001 --target windows
  MSVC error C2065: 'M_PI' undeclared in reverb.cpp:42

# Fix the issue, commit
$ shipyard run --targets windows
  windows = pass

$ shipyard ship
  PR #42 → Merged
```

Shipyard remembers the evidence from the previous run. When you re-run
just Windows and it passes, all three platforms now have green evidence
for this SHA.

### Scenario: Multiple agents working in parallel

You have two agents working in separate worktrees — one on reverb,
one on delay. Both need CI, and your machine has one Windows VM.

Shipyard's queue handles this automatically. The first agent's run starts
immediately. The second agent's run queues behind it. When the first
finishes, the second starts.

```
Agent 1 (worktree: ~/Code/my-plugin-reverb):
  shipyard ship → queued → running → PR #42 merged

Agent 2 (worktree: ~/Code/my-plugin-delay):
  shipyard ship → queued → waiting → running → PR #43 merged
```

No collisions. No manual coordination. The queue is machine-global.

### Scenario: You want to prioritize one job over another

Two jobs are queued. The delay feature is urgent. Bump it up.

```
$ shipyard queue
  Running: sy-001 feature/reverb  [normal]
  Pending: sy-002 feature/delay   [low]

$ shipyard bump sy-002 high
  Bumped sy-002 to high
```

When the current job finishes, the high-priority job runs next.

### Scenario: You want to merge to develop, not main

Your team uses a develop branch as a staging area. Ship to develop first,
promote to main later when stable.

```
$ shipyard ship --base develop
  PR #44 → Validated → Merged to develop

# Later, when develop is stable:
$ git checkout develop
$ shipyard ship --base main
  PR #45 → Validated → Merged to main
```

---

## Install

### Quick start

```bash
curl -fsSL https://generouscorp.com/Shipyard/install.sh | sh
```

Downloads a standalone binary for your platform. No runtime needed.

| OS | Architecture | Binary |
|----|-------------|--------|
| macOS | Apple Silicon (ARM64) | `shipyard-macos-arm64` |
| macOS | Intel (x64) | `shipyard-macos-x64` |
| Windows | x64 | `shipyard-windows-x64.exe` |
| Linux | x64 | `shipyard-linux-x64` |
| Linux | ARM64 | `shipyard-linux-arm64` |

### Build from source (for contributors)

```bash
git clone https://github.com/danielraffel/Shipyard.git
cd Shipyard
python3 -m venv .venv && source .venv/bin/activate
pip install -e ".[dev]"
pytest                         # verify everything works
```

### Requirements

You don't need everything — just what matches your setup:

| Tool | Required? | What it's for | Install |
|------|-----------|---------------|---------|
| [git](https://github.com/git-guides/install-git) | Yes | Version control | Pre-installed on macOS |
| `[gh](https://github.com/cli/cli)` | Yes (for PRs) | GitHub integration | `brew install gh` |
| `ssh` | For remote targets | Connect to VMs | Pre-installed on macOS not on [Linux / Ubuntu / etc](https://ubuntu.com/server/docs/how-to/security/openssh-server/) / [Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse?tabs=gui&pivots=windows-11)) |
| `[nsc](https://namespace.so/docs/reference/cli/installation)` | For Namespace | Cloud runners | `brew install namespace-cli` |
| [UTM](https://mac.getutm.app) / [Parallels](https://www.parallels.com/products/desktop/) | For VM fallback | Auto-boot VMs | `brew install --cask utm` |

`shipyard doctor` checks all of this and tells you what's missing.

---

## Quick Reference

```bash
# Setup
shipyard init                  # configure project
shipyard doctor                # check environment + suggest fixes
shipyard targets               # show targets + reachability

# Validate
shipyard run                   # full validation, all targets
shipyard run --smoke           # fast smoke check
shipyard run --targets mac     # single target

# Ship
shipyard ship                  # PR → validate → merge on green
shipyard ship --base develop   # target a different branch

# Monitor
shipyard status                # dashboard: queue + targets + evidence
shipyard queue                 # show all jobs with priorities
shipyard logs <id>             # per-target logs
shipyard evidence              # last-good SHA per platform

# Manage
shipyard bump <id> high        # reprioritize a pending job
shipyard cancel <id>           # cancel a job
shipyard cleanup --apply       # prune old logs and artifacts
```

---

## Profiles

Once you're comfortable with Shipyard, profiles let you switch between
different setups with one command.

### The problem they solve

Some days you want local-only validation (fast, free). Other days you need
the full cross-platform proof (Mac + Windows + Linux via cloud). Editing
your config every time is annoying.

### Define profiles once

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

### Switch instantly

```bash
$ shipyard config use local          # just my Mac
$ shipyard config use normal         # Mac + Namespace cloud
$ shipyard config use full           # Mac + VMs + cloud fallback
```

### See what's active

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

### Global vs project profiles

Profiles work at both levels:

- **Global** (`~/.config/shipyard/config.toml`) — your default setups, shared
  across all projects. Define `local`, `normal`, `full` here once.
- **Project** (`.shipyard/config.toml`) — project-specific profiles that
  override or extend global ones. A project that needs ARM Linux testing
  can add a `release` profile with extra targets.

Switch profiles globally or per-project. `shipyard status` always shows
which profile is active and exactly where each target will run.

### Fallback is opt-in

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
