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
| `shipyard-launcher --launcher-active` | Whether routing may use the launcher now (activation, below) |
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

## Which jobs will use it

This change adds the bundle, its build, the check and the activation record.
No entry point uses it yet. A follow-up change routes these, each only while
`--launcher-active` says so, logging `launcher=app` or `launcher=direct
reason=...` and falling back to the direct call:

| Entry point | Planned routing |
| --- | --- |
| A LaunchAgent whose program is the Shipyard release (today `com.danielraffel.shipyard.fleet-reconcile`) | At startup Shipyard, as a launchd job's own program, `exec`s the launcher with the same arguments. `exec` keeps the job's pid, so the job's identity becomes the bundle. |
| The daemon agent (`com.danielraffel.shipyard.daemon.<id>`) | The agent plist names the bundle as its program, with the same arguments. |
| The steward tick (program `/bin/bash`) | The installed tick `exec`s `shipyard-launcher --launcher-script steward-tick`. |
| fleet-deploy | Installs a bundle staged at `~/.local/share/shipyard/launcher/staged/ShipyardLauncher.app` only when `shipyard-launcher-check --verify-bundle` accepts it. It never activates it. |

Still their own identities after that: the `com.danielraffel.fleet-pass.*`
agents (installed from fleet-handbook; they need a `fleet-pass` allowlist
entry), processes the tick detaches with `setsid` (once their responsible
process exits, macOS attributes them to themselves), and tartci's agents
(TartCILauncher.app).

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

Until fleet-deploy installs it (follow-up), install a verified bundle by hand:

```bash
mkdir -p ~/.local/share/shipyard/launcher/staged
ditto -x -k ShipyardLauncher-macos-arm64.zip ~/.local/share/shipyard/launcher/staged/
tools/shipyard-launcher/shipyard-launcher-check \
  --verify-bundle ~/.local/share/shipyard/launcher/staged/ShipyardLauncher.app \
  && ditto ~/.local/share/shipyard/launcher/staged/ShipyardLauncher.app ~/.local/libexec/ShipyardLauncher.app
```

`--verify-bundle` requires a Developer ID signature with the launcher's bundle
identifier, by the installed Shipyard's team, and `spctl` reporting
`source=Notarized Developer ID`.

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
every check passes, and only when the access answer came from a launcher whose
parent is launchd (anything else is reported as `unknown`). The record is
bound to this exact bundle: its canonical path, bundle identifier, team, code
directory hash (cdhash) and designated requirement, plus `fda_checked_at`, the
time the grant was confirmed. `--activate` then asks the launcher for its
verdict and removes the record again if the launcher does not accept it.
`--deactivate` removes it.

Nothing trusts the file by itself. Every consumer asks the launcher:

```bash
~/.local/libexec/ShipyardLauncher.app/Contents/MacOS/shipyard-launcher --launcher-active
# exit 0 {"active":true,"reason":"grant_cached"}   or   exit 1 {"active":false,"reason":"..."}
```

`--launcher-active` first does everything `--launcher-verify` does (its own
signature and the current release; failures exit 77). It then reports active
only when all of these hold:

| Check | Reason when it fails | Record |
| --- | --- | --- |
| The record exists | `not_activated` | |
| It is a regular file owned by the user, not group- or world-writable, not a symlink, in private directories up to home | `record_untrusted: ...` | kept |
| It parses, with schema 2 and a numeric `fda_checked_at` | `record_unreadable`, `record_schema` | kept |
| Its `app` is this bundle, at `~/.local/libexec/ShipyardLauncher.app` | `record_names_another_bundle` | kept |
| Its identifier and team are this bundle's own | `record_identifier_mismatch`, `record_team_mismatch` | kept |
| Its cdhash is this bundle's own (a rebuilt or replaced bundle needs `--activate` again; the grant itself carries over) | `bundle_replaced: ...` | kept |
| The grant was confirmed less than an hour ago, or is confirmed again now | `grant_unknown: ...` | kept |
| The recheck does not say the grant is gone | `grant_revoked: demoted to ...` | renamed to `activation.json.demoted-<time>` |

The recheck is the same probe the check tool uses: a fresh copy of the bundle
started through LaunchServices opens the system TCC database. It never
prompts, never touches `/Volumes`, and takes at most 20 seconds. Its answer
counts only when that copy's parent is launchd. A confirmed grant refreshes
`fda_checked_at`, so the probe runs at most about once an hour per host. A
clock that moved backwards counts as stale. Every verdict is one line in
`~/Library/Logs/shipyard-launcher.log`.

## Tests

```bash
python3 -m unittest tools/shipyard-launcher/test_launcher.py        # compiles main.swift; macOS only
python3 -m unittest tools/shipyard-launcher/test_launcher_check.py
python3 -m unittest tools/shipyard-launcher/test_build_launcher.py
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
- that macOS attributes a launchd job run through the bundle, and the
  bundle's children, to `com.danielraffel.shipyard.launcher` (the PR checklist
  has the exact steps);
- that the Full Disk Access grant survives a launcher rebuild and a Shipyard
  release, which is the point of the bundle.
