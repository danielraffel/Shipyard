# ShipyardLauncher.app

A small, stable, Developer ID signed app bundle that Shipyard's launchd jobs
run under on macOS, so that macOS privacy consent (TCC) sees one identity
across every Shipyard release.

## Why

macOS decides privacy access (here: Removable Volumes, for `/Volumes/*`) for a
process's *responsible process*. A launchd job is its own responsible process
and its children inherit it. A bare command-line binary is identified by its
path, and every Shipyard install puts the release at a new path,
`~/.local/share/shipyard/auth-generations/<id>/shipyard`. A LaunchAgent whose
program is that binary (or the `~/.local/bin/shipyard` symlink to it) is
therefore a new identity after every install, and its first open of an
external volume raises an "access removable volume" alert. Nobody answers it on
an unattended host, and launchd jobs that need the volume block until someone
does. On m3 that stalled the fleet for hours on 2026-11-13 (tartci issue 840;
pulp-planning `architecture/2026-11-13-m3-tcc-wedge-root-cause.md`).

An app bundle is identified by its designated requirement: bundle identifier
`com.danielraffel.shipyard.launcher` plus the Developer ID team. That survives
rebuilds and re-signing, so one Full Disk Access grant lasts.

The design follows TartCI's `TartCILauncher.app`.

## What it does

`Contents/MacOS/shipyard-launcher` (Swift, `main.swift`):

| Invocation | Effect |
| --- | --- |
| `shipyard-launcher <args...>` | Runs the current Shipyard release with `<args...>` as a child |
| `shipyard-launcher --launcher-script steward-tick [args...]` | Runs `~/.local/libexec/shipyard-steward-tick.sh` with `/bin/bash` as a child |
| `shipyard-launcher --launcher-verify` | Checks everything a run would check, prints JSON, runs nothing |
| `shipyard-launcher --launcher-check-access [--launcher-result FILE]` | Reports whether the launcher holds Full Disk Access (read-only) |
| `shipyard-launcher --launcher-version` | Prints the launcher version |

Rules, all fail closed (exit 77, nothing runs):

- The launcher verifies its own bundle signature and reads its own Team ID.
  An unsigned or ad-hoc launcher has no team and refuses everything.
- The target is never taken from arguments or the environment. The home
  directory comes from the user database, not `$HOME`. The release is what
  `~/.local/bin/shipyard` resolves to, and it must be
  `auth-generations/<64 hex>/shipyard` or a regular file at the pointer itself
  (an `install.sh` host). The file and every directory up to home must be owned
  by the user (or root) and not group- or world-writable, with no symlink after
  resolution.
- The release must satisfy: Apple anchor, Developer ID CA intermediate,
  Developer ID Application leaf, leaf OU equal to the launcher's own team, and
  codesign identifier `shipyard`. Apple Development certificates of the same
  team are refused.
- The release is spawned suspended (`POSIX_SPAWN_START_SUSPENDED`), the code
  the kernel actually loaded is checked against the same requirement, and only
  then resumed. A file swapped between the static check and the spawn is
  killed.
- Scripts come from a compiled-in allowlist (only the steward tick today).
  A shell script cannot carry a code signature, so it is trusted by the
  allowlist and the same ownership checks, which is the trust launchd already
  gives it. `BASH_ENV`, `ENV`, `SHELLOPTS`, `BASHOPTS`, `CDPATH`, `GLOBIGNORE`,
  `PS4` and exported functions are removed, and bash runs with
  `--noprofile --norc`.

Pass-through:

- Arguments are passed verbatim. Only a first argument starting with
  `--launcher-` is the launcher's own.
- The environment is passed unchanged except that `DYLD_*` is removed, and
  `SHIPYARD_LAUNCHER_APP=<bundle>` is added so nothing routes through the
  launcher twice.
- The child stays in the launcher's process group, so launchd's process-group
  handling (`AbandonProcessGroup`) is unchanged.
- SIGTERM, SIGINT, SIGHUP, SIGQUIT, SIGUSR1, SIGUSR2 and SIGWINCH are
  forwarded to the child. SIGINT and SIGQUIT are not forwarded again when the
  launcher is the terminal's foreground group, because the terminal already
  delivered them to the child.
- The child's exit code is the launcher's. A child killed by signal N kills the
  launcher with N (without a core file); 128 + N if that fails.
- The launcher spawns and waits. It never `exec`s the release: `exec` keeps
  the pid, and the pid's identity would become the release path again.
- Every run and refusal is one line in `~/Library/Logs/shipyard-launcher.log`.

Launcher exit codes: 64 usage, 70 internal error, 77 refused, 126 spawn
failed, 127 target missing.

## Which jobs use it

Routing starts only after the host is *activated* (below). Until then nothing
changes.

| Entry point | How it routes | Log |
| --- | --- | --- |
| Any LaunchAgent whose program is the Shipyard release (today `com.danielraffel.shipyard.fleet-reconcile`) | At startup Shipyard checks that it is a launchd job's own program (parent pid 1 and `XPC_SERVICE_NAME` set), not already under the launcher, and running the same file the launcher would run. It then `exec`s the launcher with the same arguments. `exec` keeps the job's pid, and the job's identity becomes the bundle (`src/launcher_app.rs`). | `shipyard-launcher.log`: `job=<label> launcher=app` or `launcher=direct reason=...` |
| The daemon agent (`com.danielraffel.shipyard.daemon.<id>`) | On `shipyard daemon refresh`, the agent plist names the bundle as its program with the same arguments; the bundle runs the current release's `daemon supervise` (`daemon_launcher::daemon_program_arguments`). Hosts without `shipyard daemon launcher install` keep spawning the daemon directly. | `shipyard-launcher.log`: `daemon=<label> launcher=app` or `launcher=direct reason=...` |
| The steward tick (`com.danielraffel.shipyard.steward-tick`, program `/bin/bash`) | The installed tick `exec`s `shipyard-launcher --launcher-script steward-tick`, which runs the same file again as its child. | `summary.log`: `launcher=app` or `launcher=direct` on every repo line |

If the launcher is activated but missing, or `--launcher-verify` refuses, each
entry point logs why and runs directly, as before. `SHIPYARD_LAUNCHER_ROUTING=off`
(Shipyard) and `SHIPYARD_STEWARD_LAUNCHER=off` (tick) turn routing off.

Not covered here, and still their own identities:

- `com.danielraffel.fleet-pass.*` (`fleet-pass.sh`, installed from
  fleet-handbook) runs agents with `--add-dir /Volumes/Workshop/Code`. It can
  use the same preamble as the steward tick once a `fleet-pass` entry is added
  to the launcher's script allowlist.
- Processes the tick starts detached with `setsid` (fleet-deploy, drain watch,
  loss report) outlive it. Once their responsible process exits, macOS
  attributes them to themselves (`/bin/bash`, `/usr/bin/python3`).
- tartci's own agents (queue tick, watchdog) belong to TartCILauncher.app.

## Build, sign and notarize (maintainer's Mac)

Signing happens on the maintainer's Mac with the same environment as
`scripts/release_macos_local.py` (`SHIPYARD_SIGNING_IDENTITY`, the keychain
variables and the notarization variables, from the same default env files).
Never in CI.

```bash
# Unsigned build plus the exact signing plan. Signs nothing.
python3 tools/shipyard-launcher/build_launcher.py --dry-run

# Developer ID, hardened runtime, notarized, stapled, spctl-gated.
python3 tools/shipyard-launcher/build_launcher.py --sign --notarize
```

The signed build writes `dist/launcher/ShipyardLauncher.app`,
`dist/launcher/ShipyardLauncher-macos-arm64.zip` and its `.sha256`, after
`codesign --verify --strict --deep` against the Developer ID requirement and
`spctl --assess --type execute` reporting `Notarized Developer ID`.

The launcher should change rarely. A rebuild keeps every host's grant as long
as the bundle identifier and team stay the same.

## Install on a host

Stage the signed bundle, and the next fleet-deploy run (the steward tick starts
it at most every 10 minutes) installs it:

```bash
mkdir -p ~/.local/share/shipyard/launcher/staged
ditto -x -k ShipyardLauncher-macos-arm64.zip ~/.local/share/shipyard/launcher/staged/
```

fleet-deploy installs it to `~/.local/libexec/ShipyardLauncher.app` only when
`shipyard-launcher-check --verify-bundle` accepts it (Developer ID, the
installed Shipyard's team, the bundle identifier, notarized), copies it with
`ditto`, verifies the copy, and keeps the previous bundle as
`ShipyardLauncher.app.bak-<stamp>`. Otherwise it logs `unit=launcher absent`
or `unit=launcher refused reason=...` once and changes nothing. Installing never
activates it.

The path sits beside `TartCILauncher.app` and the daemon's stable launcher
copy: on the internal disk, owned by the user (no sudo), and outside
`~/Applications`, so it does not show up as an app.

## Owner step: grant Full Disk Access (once per host)

At the host's console or over Screen Sharing:

1. Open System Settings > Privacy & Security > Full Disk Access.
2. Click **+**, press Cmd-Shift-G, enter
   `~/.local/libexec/ShipyardLauncher.app`, and click **Open**.
3. Turn on **Shipyard Launcher**.

Full Disk Access covers removable volumes, and it also overrides an earlier
"Don't Allow" on a Removable Volumes prompt. The grant stays across launcher
rebuilds and every Shipyard release.

## Check and activate

```bash
tools/shipyard-launcher/shipyard-launcher-check          # read-only report, exit 0 when ready
tools/shipyard-launcher/shipyard-launcher-check --json
tools/shipyard-launcher/shipyard-launcher-check --activate
tools/shipyard-launcher/shipyard-launcher-check --deactivate
```

The check verifies the installed bundle, asks the launcher to verify the
current release, and then starts the launcher through LaunchServices
(`open -n -g -a`), so the launcher is its own responsible process exactly as
under launchd. The launcher opens the system TCC database, which only a
process with Full Disk Access can open. That never raises a prompt, and nothing
touches `/Volumes`. Run it in the logged-in GUI session; over plain SSH
LaunchServices may refuse, and the check reports `unknown`.

`--activate` writes `~/.local/share/shipyard/launcher/activation.json` only when
every check passes. Routing reads only that record, so a launcher without a
grant can never raise a new unattended prompt. `--deactivate` removes it and
every job runs directly again on its next start (the daemon on its next
`shipyard daemon refresh`).

## Tests

```bash
python3 -m unittest tools/shipyard-launcher/test_launcher.py        # compiles main.swift; macOS only
python3 -m unittest tools/shipyard-launcher/test_launcher_check.py
python3 -m unittest tools/shipyard-launcher/test_build_launcher.py
python3 -m unittest tools/steward-tick/test_steward_tick.py
python3 -m unittest tools/fleet-deploy/test_fleet_deploy.py
cargo test --lib launcher
```

`test_launcher.py` builds `main.swift` twice. The test build
(`-D SHIPYARD_LAUNCHER_TESTING`) takes its home and its requirement from the
environment, so ad-hoc signed fixtures stand in for releases; it covers target
resolution, every refusal, the swap race, and argument, environment, exit
status and signal pass-through. The production build is run unsigned and
ad-hoc signed to show it refuses everything and ignores the test variables.

Only a real signed build on a real host can show:

- that a Developer ID release of the same team is accepted under the real
  requirement, and that `build_launcher.py --sign --notarize` produces a bundle
  `spctl` accepts as `Notarized Developer ID`;
- that macOS attributes a job routed by `exec` into the bundle, and the
  bundle's children, to `com.danielraffel.shipyard.launcher` (check with
  `/usr/bin/log show --info --predicate 'process == "tccd"'` around a routed
  job's first `/Volumes` access, or with `responsibility_get_pid_responsible_for_pid`);
- that the Full Disk Access grant survives a launcher rebuild and a Shipyard
  release, which is the point of the bundle.
