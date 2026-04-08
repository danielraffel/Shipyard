# Shipyard

Cross-platform CI from your machine. Validate commits on local VMs, SSH hosts,
and cloud runners — with one config file, automatic failover, and structured
output for AI agents.

```bash
curl -fsSL https://raw.githubusercontent.com/danielraffel/Shipyard/main/install.sh | sh
cd my-project
shipyard init        # detects your project, probes your machines
shipyard run         # validates on every platform you configured
```

---

## What It Does

You have a Mac. Maybe you have a Windows VM and a Linux VM running on it.
Maybe you have cloud runner accounts on Namespace or GitHub. Shipyard
coordinates all of them to validate your code before you merge.

- **Local builds** run directly on your Mac — fast, no network needed
- **Remote builds** run on your VMs over SSH — real Windows and Linux, not
  emulated
- **Cloud builds** dispatch to Namespace or GitHub Actions — fallback when
  your VMs are off, or when you need neutral hardware

When you run `shipyard run`, it delivers the exact commit to each machine,
runs your build and test commands, and tells you what passed. When a machine
is unreachable, Shipyard automatically tries the next one in the chain —
boot the VM, or dispatch to cloud runners.

When everything is green, `shipyard ship` creates a PR and merges it.

## What It Doesn't Do

Shipyard is not a CI service, not a build system, not a workflow engine. It
calls your build commands and cares about one thing: did they pass on every
platform?

---

## Examples

### You're building a macOS and iOS app

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

### You're building a cross-platform audio plugin

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

### You're building a macOS desktop app

Single platform, single machine. You still get Shipyard's queue (so parallel
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

### You're building a cross-platform Tauri app

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

Every target in your config is a real machine, not an abstract label:

| Target label | What it means | How Shipyard reaches it |
|-------------|---------------|------------------------|
| `mac` | Your Mac | Runs commands directly (local) |
| `ubuntu` | Your Ubuntu VM | Connects via SSH, sends code as a git bundle |
| `windows` | Your Windows VM | Connects via SSH, runs PowerShell commands |
| `cloud-linux` | A Namespace runner | Dispatches a GitHub Actions workflow |

When a target is unreachable, Shipyard walks a fallback chain:

```
1. Try SSH → unreachable
2. Try booting the VM (UTM) → boot, wait for SSH
3. Try cloud (Namespace) → dispatch GitHub Actions
4. Try cloud (GitHub-hosted) → last resort
```

The chain is configurable per target. You can skip VMs, skip cloud, or make
cloud the primary. The default is: local first, VM fallback, cloud last resort.

---

## How It Works With Agents

Every command supports `--json` for structured output. AI agents (Claude Code,
Codex) call the same CLI humans use and parse the JSON result.

### Two modes you can drop in

**Mode 1: CI on push, manual merge** (default) —
Validation runs when you push. You review and merge yourself.

**Mode 2: CI on push, auto-merge on green** —
Validation runs. When all platforms pass, the PR is merged automatically.

---

## Claude Code Integration

Shipyard ships integration files you drop into your project. Pick what fits.

### Option A: CLAUDE.md snippet (simplest)

Add to your `CLAUDE.md`:

```markdown
## CI

This project uses Shipyard for cross-platform CI.

Before merging: `shipyard run` — wait for all targets green.
To ship: `shipyard ship` — creates PR, validates, merges on green.
Status: `shipyard status` / `shipyard evidence` / `shipyard logs <id>`
```

### Option B: Agent hook (automated CI after push)

Add to `.claude/settings.json`:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Bash",
        "command": "if echo \"$TOOL_INPUT\" | grep -q 'git push'; then echo '[Shipyard] CI triggered'; shipyard run --json 2>/dev/null || true; fi"
      }
    ]
  }
}
```

### Option C: Merge-on-green agent (fully automated)

Add `.claude/agents/ci.md`:

```markdown
---
name: ci
description: Runs cross-platform CI validation and merges on green
tools: [Bash, Read]
---

When asked to ship, land, or merge code:
1. Ensure changes are on a feature branch (never main)
2. Run: shipyard ship --json
3. If all targets pass, the PR is merged automatically
4. If any target fails, report the failure and suggest fixes

When asked to check CI: shipyard status --json
When asked about evidence: shipyard evidence --json
```

---

## Workflow Examples

### Feature branch → CI → merge

```
$ shipyard run                    # validate on all platforms
  mac = pass, ubuntu = pass, windows = pass

$ shipyard ship                   # create PR, merge on green
  PR #42 → All green → Merged to main
```

### CI fails → fix → re-run just the failed target

```
$ shipyard run
  mac = pass, ubuntu = pass, windows = FAIL

$ shipyard logs sy-001 --target windows
  MSVC error C2065: 'M_PI' undeclared...

# Fix the issue, commit
$ shipyard run --targets windows   # only re-validate Windows
  windows = pass

$ shipyard ship                    # now merge
```

### Queue management

```
$ shipyard queue                   # see what's queued
  Running: sy-001 feature/reverb @ abc1234 [normal]
  Pending: sy-002 feature/delay  @ def5678 [low]

$ shipyard bump sy-002 high        # move it up in priority
$ shipyard cancel sy-001           # cancel the running job
```

### Fully automated (agent does everything)

```
You: "Ship the reverb feature to main"

Agent:
  → shipyard ship --json
  → mac = pass, ubuntu = pass, windows = pass
  → PR #42 merged. All 3 platforms passed.
```

---

## Install

### Quick start

```bash
curl -fsSL https://raw.githubusercontent.com/danielraffel/Shipyard/main/install.sh | sh
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
```

This installs Shipyard in editable mode so you can modify and test it locally.

## Requirements

- git
- `gh` CLI for GitHub integration (`brew install gh`)
- `nsc` CLI for Namespace cloud runners (optional — `brew install namespace-cli`)
- SSH access to any remote VMs you want to validate on
- UTM, Parallels, or Tart for local VM management (optional)

---

## Quick Reference

```bash
# Setup
shipyard init                  # configure project
shipyard doctor                # check environment
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
