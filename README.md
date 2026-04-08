# Shipyard

Cross-platform CI from your machine. Validate commits on local VMs, SSH hosts,
and cloud runners — with one config file, automatic failover, and structured
output for AI agents.

```bash
curl -fsSL https://raw.githubusercontent.com/danielraffel/shipyard/main/install.sh | sh
cd my-project
shipyard init        # detects your project, probes your machines
shipyard run         # validates on every platform you configured
```

---

## What It Does

Shipyard coordinates validation across the machines you already have:

- Your **local Mac** runs macOS builds directly
- Your **UTM/Parallels VMs** run Windows and Linux builds over SSH
- **Namespace** or **GitHub Actions** runners handle cloud fallback

When you run `shipyard run`, it delivers the exact commit SHA to each target,
runs your validation command, and reports structured results. When a target is
unreachable, it falls over to the next backend automatically.

When everything is green, `shipyard ship` creates a PR and merges it.

## What It Doesn't Do

Shipyard is not a CI service, not a build system, not a workflow engine. It
calls your build commands and cares about one thing: did they pass on every
platform?

---

## Adding Shipyard to Your Projects

### iOS App (Swift + Xcode)

```
$ cd my-ios-app
$ shipyard init

Detecting project...
  Found: MyApp.xcodeproj (Xcode project)
  Found: .git (GitHub remote: danielraffel/my-ios-app)
  Platforms detected: macOS, iOS

What platforms do you want to validate?
  [x] macOS    (local Mac — Xcode 16.2 found)
  [x] iOS      (local simulator — iPhone 16 Pro available)
  [ ] Windows
  [ ] Linux

Checking accounts...
  GitHub: authenticated as danielraffel
  Namespace: authenticated (generouscorp)

Validation commands:
  Build [xcodebuild -scheme MyApp -destination 'platform=iOS Simulator,name=iPhone 16 Pro' build]:
  Test  [xcodebuild -scheme MyApp -destination 'platform=iOS Simulator,name=iPhone 16 Pro' test]:

Writing .shipyard/config.toml... done

Ready! Try: shipyard run
```

**What you get:** Every commit is validated by actually building and running
your test suite in a real Xcode simulator on your Mac. No waiting for GitHub
runners to boot macOS. No "works on CI but not locally" surprises.

If you also have a Mac on Namespace, Shipyard can dispatch there as a second
opinion or when your laptop is busy.

```
$ shipyard run
  macos   = pass  (local, 1m42s)
  ios-sim = pass  (local, 2m15s)
  All green.
```

---

### Audio Plugin (Pulp / JUCE / CMake)

```
$ cd my-synth-plugin
$ shipyard init

Detecting project...
  Found: CMakeLists.txt (CMake C++ project)
  Found: .git (GitHub remote: danielraffel/my-synth-plugin)
  Platforms detected: macOS, Windows, Linux

What platforms do you want to validate?
  [x] macOS    (local Mac)
  [x] Windows  (SSH host "win" — reachable, 23ms)
  [x] Linux    (SSH host "ubuntu" — reachable, 847ms)
  [ ] iOS
  [ ] Android

Validation commands:
  Build [cmake -S . -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build --parallel]:
  Test  [ctest --test-dir build --output-on-failure]:

Cloud failover: fall back to Namespace when VMs are down? [Y/n]
Merge policy: require all 3 platforms green? [Y/n]

Writing .shipyard/config.toml... done
Writing .github/workflows/ci.yml... done

Ready! Try: shipyard run
```

**What you get:** Audio plugins must work on macOS, Windows, and Linux. DAW
users are on all three. Shipyard validates the exact same commit on your local
Mac, your Windows VM, and your Linux VM — simultaneously.

When your VMs are asleep or unreachable, it automatically dispatches to
Namespace cloud runners. You never have to do anything.

```
$ shipyard run
  mac     = pass  (local, 3m12s)
  windows = pass  (ssh, 5m30s)
  ubuntu  = pass  (ssh, 4m18s)
  All green.

$ shipyard ship
  Created PR #42: "Add velocity-sensitive filter envelope"
  Validating...
  All platforms green. Merging.
```

---

### macOS Desktop App (SwiftUI)

```
$ cd my-mac-app
$ shipyard init

Detecting project...
  Found: Package.swift (Swift package)
  Found: .git (GitHub remote: danielraffel/my-mac-app)
  Platforms detected: macOS

What platforms do you want to validate?
  [x] macOS    (local Mac)

Validation commands:
  Build [swift build]:
  Test  [swift test]:

Writing .shipyard/config.toml... done

Ready! Try: shipyard run
```

**What you get:** The simplest case. One platform, one machine, instant
validation. But you still get the queue (so multiple worktrees don't collide),
evidence tracking (so you know what SHA last passed), and `shipyard ship` for
one-command merge.

```
$ shipyard run
  macos = pass  (local, 45s)

$ shipyard ship
  Created PR #7. Validated. Merged.
```

If you later decide you need Linux or Windows builds (maybe for a
cross-platform version), just:

```
$ shipyard targets add ubuntu
  SSH host "ubuntu" — reachable. Added.
$ shipyard targets add windows
  SSH host "win" — reachable. Added.
```

No re-init needed.

---

### Cross-Platform Tauri App (Rust + TypeScript)

```
$ cd my-tauri-app
$ shipyard init

Detecting project...
  Found: Cargo.toml (Rust project)
  Found: package.json (Node.js — likely frontend)
  Found: src-tauri/ (Tauri app detected)
  Found: .git (GitHub remote: danielraffel/my-tauri-app)
  Platforms detected: macOS, Windows, Linux

What platforms do you want to validate?
  [x] macOS    (local Mac)
  [x] Windows  (SSH host "win" — reachable)
  [x] Linux    (SSH host "ubuntu" — reachable)

Validation commands:
  Build [npm ci && cd src-tauri && cargo build]:
  Test  [cd src-tauri && cargo test]:

Cloud failover: fall back to Namespace when VMs are down? [Y/n]

Writing .shipyard/config.toml... done
Writing .github/workflows/ci.yml... done

Ready! Try: shipyard run
```

**What you get:** Tauri apps ship native binaries on all three platforms. The
Rust backend and the platform-specific windowing code both need to compile and
pass tests on each OS. Shipyard validates all three in parallel.

```
$ shipyard run
  mac     = pass  (local, 2m08s)
  ubuntu  = pass  (ssh, 3m45s)
  windows → SSH unreachable → booting VM "Windows 11"...
          = pass  (utm-fallback, 6m30s)
  All green.
```

The Windows VM was asleep. Shipyard booted it via UTM, waited for SSH to come
up, ran the build, and reported the result. You didn't have to do anything.

---

## How It Works With Agents

Shipyard is designed to be operated by AI coding agents (Claude Code, Codex)
as naturally as by humans. Every command supports `--json` for structured
output, and the project ships integration files that make CI automatic.

### Two modes you can drop in

**Mode 1: CI on push, manual merge** (default) —
Validation runs automatically when you push to a branch. You review and merge
yourself.

**Mode 2: CI on push, auto-merge on green** —
Validation runs automatically. When all platforms pass, the PR is merged
without intervention.

Both modes work with feature branches, develop branches, and worktrees.

---

## Claude Code Integration

Shipyard ships files you drop into your project that teach Claude Code how to
use CI. Pick what fits your workflow.

### Option A: CLAUDE.md snippet (simplest)

Add this to your project's `CLAUDE.md`:

```markdown
## CI

This project uses Shipyard for cross-platform CI.

### Before merging to main

Always validate before merging:

    shipyard run

Wait for all targets to pass. Do not merge with any target red or unreachable.

### Creating a PR

    shipyard ship

This creates a PR, validates on all platforms, and reports results. Do not
push directly to main.

### Checking CI status

    shipyard status          # queue and recent results
    shipyard evidence        # last-good SHA per target
    shipyard logs <job-id>   # if something failed
```

This is enough for most projects. Claude will follow the instructions when
it's time to merge.

### Option B: Agent hook (automated CI on branch push)

For automated CI that runs every time code is pushed to a branch, add a hook
to `.claude/settings.json`:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Bash",
        "command": "if echo \"$TOOL_INPUT\" | grep -q 'git push'; then echo '[shipyard] CI triggered'; shipyard run --json 2>/dev/null || true; fi"
      }
    ]
  }
}
```

This triggers `shipyard run` after any `git push`. The `--json` output lands
in the conversation so the agent can see results and react to failures.

### Option C: Merge-on-green agent (fully automated)

For the full autonomous workflow — push triggers CI, green triggers merge —
add this agent file:

**`.claude/agents/ci.md`:**

```markdown
---
name: ci
description: Runs cross-platform CI validation and merges on green
tools: [Bash, Read]
---

When asked to ship, land, or merge code:

1. Ensure changes are committed and pushed to a feature branch (never main)
2. Run: shipyard ship --json
3. This will:
   - Create a PR if one doesn't exist
   - Validate on all configured platforms
   - Report per-target results
4. If all targets pass, the PR is merged automatically
5. If any target fails, report the failure and suggest fixes
6. Do not retry more than once without asking the user

When asked to check CI:
  Run: shipyard status --json

When asked about what passed:
  Run: shipyard evidence --json
```

Use it by saying: "ship this to main" or "land this feature"

### Option D: Skill file (for Claude Code plugin projects)

**`.claude/skills/ci.md`:**

```markdown
---
name: ci
description: Cross-platform CI validation via Shipyard
---

## Validate current branch

    shipyard run

Shows live progress. Blocks until all targets report.

## Ship to main (or any base branch)

    shipyard ship                  # default: PR to main
    shipyard ship --base develop   # PR to develop instead

Creates PR, validates all platforms, merges on green.

## Ship to develop, then later to main

    shipyard ship --base develop   # feature → develop
    # (later, when develop is stable)
    git checkout develop
    shipyard ship --base main      # develop → main

## Check status

    shipyard status            # queue + active runs
    shipyard evidence          # last-good SHA per platform
    shipyard logs <id>         # per-target logs

## Validate a specific PR

    shipyard check <PR#>

## Key rules

- Never push directly to main
- Always validate before merging
- If a target fails, fix and re-run before merging
- Mixed evidence (local + cloud) is acceptable
- All configured platforms must be green
```

---

## Workflow Examples

### Feature branch → CI → merge

The most common flow. You're on a feature branch, ready to land.

```
feature/add-reverb
  │
  ├── shipyard run              # validate on all platforms
  │     mac     = pass
  │     ubuntu  = pass
  │     windows = pass
  │
  ├── shipyard ship             # create PR, merge on green
  │     PR #42 created
  │     All platforms green
  │     Merged to main
  │
  └── (branch deleted)
```

### Worktree → CI → merge

You're working in a worktree (common with Claude Code's parallel agents).

```
main repo: ~/Code/my-plugin          (on main)
worktree:  ~/Code/my-plugin-reverb   (on feature/add-reverb)

$ cd ~/Code/my-plugin-reverb
$ shipyard run
  mac     = pass  (local, 3m12s)
  ubuntu  = pass  (ssh, 5m30s)
  windows = pass  (ssh, 4m18s)

$ shipyard ship
  PR #42 → merged to main
```

Shipyard's queue is machine-global, so multiple worktrees coordinate
automatically. If two worktrees submit runs at the same time, they queue
instead of colliding.

### Feature → develop → main (integration branch)

For projects that use a develop branch as a staging area:

```
feature/add-reverb
  │
  ├── shipyard ship --base develop    # PR to develop, not main
  │     All green. Merged to develop.
  │
  └── (later, when develop is stable)
      $ git checkout develop
      $ shipyard ship --base main     # PR develop → main
        All green. Merged to main.
```

### CI fails → fix → re-run (targeted)

```
$ shipyard run
  mac     = pass  (local, 3m12s)
  ubuntu  = pass  (ssh, 5m30s)
  windows = FAIL  (ssh, 4m18s)

$ shipyard logs sy-001 --target windows
  ... MSVC error C2065: 'M_PI' undeclared ...

# Fix the issue, commit
$ shipyard run --targets windows    # only re-validate the failed target
  windows = pass  (ssh, 4m05s)

$ shipyard ship
  All green. Merged.
```

### Fully automated (agent does everything)

With the merge-on-green agent installed:

```
You: "Ship the reverb feature to main"

Agent:
  → git push origin feature/add-reverb
  → shipyard ship --json
  → PR #42 created
  → Validating...
  →   mac     = pass
  →   ubuntu  = pass
  →   windows = pass
  → All green. Merged to main.
  → "Done. PR #42 merged. All 3 platforms passed."
```

If CI fails, the agent reports the failure and can attempt a fix:

```
Agent:
  → shipyard ship --json
  → windows = FAIL
  → shipyard logs sy-002 --target windows --json
  → "Windows failed: MSVC error C2065 in reverb.cpp:42"
  → (reads file, fixes the issue, commits)
  → shipyard run --targets windows --json
  → windows = pass
  → shipyard ship --json
  → Merged.
```

---

## What Ships in the Box

| Component | What it does |
|-----------|-------------|
| `shipyard` CLI | Everything. Init, run, status, ship, config, doctor. |
| `sy` alias | Short form. `sy run` = `shipyard run`. |
| `.shipyard/config.toml` | Per-project config (generated by `init`). |
| `.github/workflows/ci.yml` | GitHub Actions workflow (generated by `init`, optional). |
| Claude Code integration | CLAUDE.md snippet, agent, skill, hook — pick what you need. |
| `--json` on every command | Structured output for agents. No separate API needed. |

---

## Install

### Quick start (recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/danielraffel/shipyard/main/install.sh | sh
```

Downloads a standalone binary for your OS and architecture. No Python or other
runtime needed.

Supported platforms:

| OS | Architecture | Binary |
|----|-------------|--------|
| macOS | Apple Silicon (ARM64) | `shipyard-macos-arm64` |
| macOS | Intel (x64) | `shipyard-macos-x64` |
| Windows | x64 | `shipyard-windows-x64.exe` |
| Windows | ARM64 | `shipyard-windows-arm64.exe` |
| Linux | x64 | `shipyard-linux-x64` |
| Linux | ARM64 | `shipyard-linux-arm64` |

### From source

```bash
git clone https://github.com/danielraffel/shipyard
cd shipyard && pip install -e .
```

### Via uv or pipx

```bash
uv tool install shipyard
# or
pipx install shipyard
```

## Requirements

- git
- `gh` CLI (for GitHub integration — `brew install gh`)
- `nsc` CLI (for Namespace cloud runners — optional)
- SSH access to any remote targets you configure
- UTM, Parallels, or Tart (for local VM fallback — optional)

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
shipyard check                 # smoke on local only

# Ship
shipyard ship                  # PR → validate → merge on green
shipyard ship --base develop   # target a different branch
shipyard check 42              # validate existing PR #42

# Monitor
shipyard status                # queue + active runs
shipyard logs <id>             # per-target logs
shipyard evidence              # last-good SHA per platform

# Config
shipyard config show           # effective config
shipyard config profile use x  # switch environment profile
shipyard targets add ubuntu    # add a target
```
