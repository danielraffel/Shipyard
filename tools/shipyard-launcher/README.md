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
  A shell script cannot carry a code signature, so it is bound by content
  instead: the activation record holds its SHA-256, which fleet-deploy
  records (`shipyard-launcher-check --bind-script`) only when the installed
  file is byte-identical to the tick it installed from main. On every run the
  launcher re-hashes the file, copies the verified bytes into a private
  directory and runs that copy, so a write after the check changes nothing.
  If anything is off (no bound record, a different hash, an untrusted or
  symlinked file), it does not run the script under its identity: it `exec`s
  plain `/bin/bash` on the installed file with the caller's environment and
  `SHIPYARD_LAUNCHER_FALLBACK=launcher-script-mismatch`, which is what the
  LaunchAgent would run without the launcher, and logs
  `launcher-script-mismatch`. `BASH_ENV`, `ENV`, `SHELLOPTS`, `BASHOPTS`,
  `CDPATH`, `GLOBIGNORE`, `PS4` and exported functions are removed from a
  verified run, and bash runs with `--noprofile --norc`.

- Downgrades fall back. Before running a release the launcher runs its
  `--version` the same verified way (spawned suspended, checked, resumed,
  bounded at 15 s) and keeps the newest version it has run in
  `~/.local/share/shipyard/launcher/release-floor.json`. A release older than
  that, a version it cannot read, or a floor file it cannot trust is not run
  under the launcher: it `exec`s the release directly with the caller's
  environment and `SHIPYARD_LAUNCHER_FALLBACK=launcher-release-fallback`
  (which also stops Shipyard routing again) and logs
  `launcher-release-fallback`. `--launcher-active` reports such a release as
  `release_downgrade`. A newer release passes at once. There is deliberately
  no exact path or cdhash pin: every install changes both, so a pin would make
  each new release fall back to its own per-generation identity until someone
  re-recorded it, which is the prompt this bundle exists to prevent. The
  Developer ID, team, identifier and `auth-generations` checks already limit
  the choice to genuine Shipyard releases.

Pass-through:

- Arguments are passed verbatim. Only a first argument starting with
  `--launcher-` is the launcher's own.
- The environment is passed unchanged except that `DYLD_*` is removed,
  `SHIPYARD_LAUNCHER_APP=<bundle>` is added so nothing routes through the
  launcher twice, and `PATH` is set by the launcher, never taken from the
  caller. A script gets `/usr/bin:/bin:/usr/sbin:/sbin` plus the bundle's
  `Contents/MacOS`. Shipyard gets its fixed unattended tool path
  (`/opt/homebrew/bin:/usr/local/bin:~/.local/bin` then the system
  directories), the list its LaunchAgents already use, because it finds the
  guarded `gh` wrapper by name. The steward tick sets its own documented
  `PATH` on its first line for the same reason: `gh` must resolve to the
  ghapp wrapper in `~/.local/bin`.
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

Each entry point routes only while `shipyard-launcher --launcher-active`
answers active (see Check and activate). On any other answer, or with no
launcher installed, it runs exactly as before and logs why.

| Entry point | How it routes | Log |
| --- | --- | --- |
| A LaunchAgent whose program is the Shipyard release (today `com.danielraffel.shipyard.fleet-reconcile`) | At startup Shipyard checks it is a launchd job's own program (parent pid 1, `XPC_SERVICE_NAME` set), not already under the launcher, not the daemon itself, and the same file `~/.local/bin/shipyard` resolves to. The `--launcher-active` check is bounded at 45 s. It then `exec`s the launcher with the same arguments; `exec` keeps the job's pid, so the job's identity becomes the bundle (`src/launcher_app.rs`). A failed `exec` is logged and Shipyard runs directly. | `shipyard-launcher.log`: `job=<label> launcher=app`, or `launcher=direct reason=...` |
| The steward tick (program `/bin/bash`) | The installed tick asks `--launcher-active` (bounded, `SHIPYARD_STEWARD_LAUNCHER_TIMEOUT_S`, default 45 s), then `exec`s `shipyard-launcher --launcher-script steward-tick`, which runs the same file again as its child. A timeout or a failed `exec` runs the tick directly. | `summary.log`: `launcher=app` or `launcher=direct` on every repo line, plus a `reason=not_active` line |
| fleet-deploy (`unit=launcher`) | Installs a bundle staged at `~/.local/share/shipyard/launcher/staged/ShipyardLauncher.app` only when `shipyard-launcher-check --verify-bundle` accepts it and its copy, and never one whose CFBundleVersion is below the installed one or the highest it ever installed (`launcher.version-floor`). Then, when the installed launcher is not active, runs `shipyard-launcher-check --activate`, which succeeds only once the owner's grant is in place. It never activates a bundle below the floor. Each call is bounded (`FLEET_DEPLOY_LAUNCHER_TIMEOUT_S`, default 45 s). | `summary.log`: `unit=launcher ...`, `unit=launcher-activation activated` or `waiting reason=full_disk_access=denied` |

`SHIPYARD_LAUNCHER_ROUTING=off` (Shipyard), `SHIPYARD_STEWARD_LAUNCHER=off`
(tick) and `SHIPYARD_LAUNCHER_AUTO_ACTIVATE=off` (fleet-deploy) turn routing
off.

The daemon agent (`com.danielraffel.shipyard.daemon.<id>`) is never routed.
launchd restarts it at login and after any failure, so it keeps its stable
launcher copy (`~/.local/libexec/shipyard/shipyard-daemon-launcher`) as its
program, and no state of this bundle (missing, refusing, not active) can keep
it down. Shipyard's startup routing skips `daemon run` and `daemon supervise`
for the same reason.

Still their own identities: the `com.danielraffel.fleet-pass.*` agents
(installed from fleet-handbook; they need a `fleet-pass` allowlist entry),
processes the tick detaches with `setsid` (once their responsible process
exits, macOS attributes them to themselves), and tartci's agents
(TartCILauncher.app).

## What the steward tick needs

The tick sets `PATH=~/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin`
on its first line and refuses to run unless `gh` resolves to
`~/.local/bin/gh` (the ghapp wrapper, so admissions stay guarded). It uses:
`~/.local/bin/shipyard`, `~/.local/bin/ghapp`, `jq`, `timeout` (Homebrew
coreutils), `git`, `/usr/bin/python3`, and system `tar`, `find`, `stat`,
`date`, `mktemp`, `head`, `tr`, `grep`, `cat`, `awk`. It starts
`~/.local/libexec/shipyard-fleet-deploy.sh` and two Python scripts from the
deploy clone, detached.

## What Full Disk Access reaches once active

The launcher itself gains no authority beyond running these. Every child
inherits its Full Disk Access:

| Child | How it is bound | What it can still reach |
| --- | --- | --- |
| The Shipyard release (`~/.local/bin/shipyard` resolved) | Developer ID, team `95CX6P84C4`, identifier `shipyard`, under `auth-generations/<id>/`, checked on the file and again on the suspended process; never older than the newest version run here (the floor) | Every tool Shipyard runs: `gh` (through the ghapp wrapper), `git`, `ssh`, `python3`, from its fixed tool path, which includes the user-writable `~/.local/bin` and `/opt/homebrew/bin` |
| The steward tick (`~/.local/libexec/shipyard-steward-tick.sh`) | SHA-256 bound at deploy time; a verified private copy runs | `~/.local/bin/ghapp`, `~/.local/bin/shipyard`, `jq`, `timeout`, `git`, `/usr/bin/python3`, and the scripts the tick starts (fleet-deploy, drain watch, loss report; detached, so they keep the grant only while the tick runs) |

The bindings hold against changes made by anything that does not also rewrite
`~/.local/share/shipyard/launcher/activation.json`. That record, the tick's
tools and the Homebrew and `~/.local/bin` directories are writable by the same
user, so a process running as that user can still reach Full Disk Access
through them. Closing that needs an anchor the user cannot write: the tick
sealed inside the signed bundle (every tick change would then need a new
signed launcher, so it is not done), or a root-owned record. The defence for
untrusted code is to keep it off the host: the repair lane's author filter and
running untrusted work in tartci VMs. Weigh this before granting.

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

Stage the signed bundle; fleet-deploy (started by the steward tick at most
every 10 minutes) verifies and installs it, then activates it once the grant
is in place:

```bash
mkdir -p ~/.local/share/shipyard/launcher/staged
ditto -x -k ShipyardLauncher-macos-arm64.zip ~/.local/share/shipyard/launcher/staged/
```

`--verify-bundle` requires a Developer ID signature with the launcher's bundle
identifier, by the pinned team `95CX6P84C4` (a constant in the check tool and
in the launcher, never read from `~/.local/bin/shipyard`), an unmodified
bundle, `spctl` reporting `source=Notarized Developer ID`, and an integer
`CFBundleVersion`. Raise `CFBundleVersion` in `Info.plist` for every new
launcher build: fleet-deploy installs only versions at or above the highest it
has installed.

The check tool honours its `SHIPYARD_LAUNCHER_CHECK_*` overrides (tools, home,
team, requirement) only with `SHIPYARD_LAUNCHER_CHECK_TEST_MODE=1`, which only
the tests set.

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
