# Examples

Real-world Shipyard setups for different kinds of projects.

## What your project needs

Shipyard runs your existing build and test commands on each platform. It
assumes your project already has:

- A build system (CMake, Xcode, Cargo, npm, Gradle, Swift, etc.)
- Test commands that exit 0 on success and non-zero on failure

If your project builds and has tests, Shipyard can validate it. If it
doesn't have tests yet, Shipyard still validates that the build succeeds.

---

## Scenario 1: macOS and iOS app

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

## Scenario 2: Cross-platform audio plugin

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

Cloud failover: fall back to GitHub-hosted Actions when VMs are down? [Y/n]

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

If your VMs are powered off or unreachable, Shipyard can fall back to
GitHub-hosted cloud runners — you don't have to do anything:

```
$ shipyard run
  mac     = pass  (local, 3m12s)
  windows → SSH unreachable → dispatching to GitHub Actions...
          = pass  (github-hosted-failover, 8m45s)    ← cloud runner took over
  ubuntu  = pass  (ssh, 4m18s)
  All green.
```

---

## Scenario 3: macOS desktop app with parallel agents

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

## Scenario 4: Cross-platform Tauri app

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
  windows → SSH unreachable → falling back to github-hosted...
          = pass  (cloud, 6m30s)    ← the target's fallback chain
  All green.
```

The Windows VM was off, so Shipyard used the next entry in the target's
fallback chain and reported the result. Shipyard does not boot VMs itself.
