# Self-Hosted Runner Provisioning

`shipyard runner register | list | remove | tag` brings a Mac into a repo's
GitHub Actions CI fleet and lets you see/manage that fleet across machines. It
is the provisioning complement to the [runner watchdog](./runner-watchdog.md),
which recovers stuck runners. Pure naming/index/label/table logic lives in
`src/runner_provision.rs`; the shell side (talks to `gh`, the runner's
`config.sh`/`svc.sh`, and local `~/actions-runner-*` dirs) is
`src/app/runner_provision_cmd.rs`.

## Mental model

- **GitHub is the cross-machine registry.** Every runner on every Mac reports
  to GitHub, so "X jobs on the Studio, Y on the laptop" is just "register X
  runner services on the Studio and Y on the laptop" — GitHub schedules each
  queued job onto any idle runner whose labels match. No controller, no
  inbound networking between your Macs.
- **Two phases.** Provisioning the *host* (Xcode, Homebrew deps, caches, Skia)
  is once per machine and is the repo's own concern (e.g. pulp's
  `tools/ci/bootstrap-macos-host.sh`). Registering *runners* is once per repo
  per machine and is what this command does. It assumes a buildable host.
- **Non-macOS + emulated lanes are tartci targets, not `shipyard runner`
  registrations.** This command provisions native macOS Actions runners. Local
  Linux / Windows build VMs (and the emulated **x86_64** smoke lane —
  cross-compile + qemu-user / Prism) are driven by
  [tartci](https://github.com/danielraffel/tartci) and wired into Shipyard as
  ordinary `backend = "local"` targets whose validation command shells to
  `tartci up <os> [--target-arch x86_64]` — see
  [targets.md](./targets.md#emulated-x86_64-smoke-local-via-tartci).
- **Coordination does not imply one provider.** Shipyard can supervise TartCI
  VMs on Apple Silicon, an independently managed Proxmox x64 Linux pool, and a
  native Intel Mac in one validation plan. The provider keeps responsibility
  for boot, isolation, and teardown; Shipyard supplies queue, exact-head, and
  policy visibility across them.

### Why runner-group read access matters

For organization runner groups, an external policy verifier using Shipyard's
GitHub App identity requires the App's **Self-hosted runners: Read-only**
organization permission. Repository `Actions` permission is insufficient. With
runner-group access, that verifier can check not
only whether a runner is online, but whether the group is still limited to the
intended repositories and workflows and contains the expected runners. This is
an operator integration, not currently a built-in `shipyard runner` check.

Runner-group policy is only one authorization control; it is not a sandbox. A
permitted workflow can still execute attacker-controlled PR code. Untrusted work
requires disposable guests with no host credentials or writable host mounts,
plus the repository's fork/approval policy. Persistent or secret-bearing hosts
should accept only protected, main-owned workflows that do not execute untrusted
changes.

Grant Read & write only when Shipyard must configure groups or remove
registrations. After changing the App permission, approve the installation
update and mint a new token; cached tokens keep their prior permissions. Full
setup and 403 diagnosis are in
[`github-app-quota.md`](./github-app-quota.md#register-the-github-app).

## Machine tag

Runners are named `<repo>-<machine-tag>-NN`, e.g. `pulp-studio-01`. The tag is
an explicit per-box value stored at `<state_dir>/machine-tag`, **never derived
from the hostname** — two MacBook Pros can share a hostname, so a
hostname-derived tag would collide and clobber runner names.

```bash
shipyard runner tag --set studio   # one-time, per machine (studio | m1 | m5 | …)
shipyard runner tag                # print the stored tag
```

Tags must be lowercase letters, digits, and dashes (no leading/trailing/doubled
dash).

## Register

```bash
shipyard runner register --repo Generous-Corp/pulp --count 3 \
  --ci-root /Volumes/Workshop/ci/pulp [--machine-tag studio] [--labels a,b,c] [--dry-run]
```

What it does per runner:

1. Computes the next free index by listing the repo's existing runners (any
   machine) and continuing past the highest `<repo>-<tag>-NN`. Re-running
   appends capacity (`-04`, `-05`) without collisions.
2. Downloads the fleet-pinned `osx-arm64` runner tarball once into
   `<ci-root>/cache/actions-runner-pkg/` and extracts it into
   `~/actions-runner-<name>`. Registration uses `--disableupdate`; upgrades are
   an explicit fleet-wide pin change, never a per-host automatic update.
3. Writes a per-runner `.env` pointing ccache + FetchContent at the shared
   caches and isolating each runner's `_work` for cross-worktree cache hits
   (`CCACHE_BASEDIR` + `CCACHE_NOHASHDIR`). Depend mode is forced off and the
   compiler key is content-based. Cache **size** is owned by the host's
   `ccache.conf`, not this command.
4. Installs Rust into runner-private `_toolcache/{rustup,cargo}` directories on
   the runner's own filesystem and writes a system-first `.path`
   (`/usr/bin:/bin:/usr/sbin:/sbin` before Homebrew). This keeps runner startup
   and tool lookup away from slow or offline Homebrew and symlinked shared-cache
   volumes.
5. Fetches a registration token, runs `config.sh --unattended --replace`, then
   `svc.sh install && svc.sh start` (a user LaunchAgent — no sudo).

### Labels

Default: `self-hosted,macos,arm64,<repo>-build,<repo>-build-<tag>`.

- `<repo>-build` — the shared routing label a repo's workflow selects (e.g.
  pulp's `PULP_LOCAL_MACOS_RUNS_ON_JSON=["self-hosted","pulp-build"]`). Carry it
  so the new runners immediately join the pool.
- `<repo>-build-<tag>` — host pin label, to force work onto one machine.

Override the whole set with `--labels` when a repo's workflow selects something
else (e.g. Shipyard's own CI currently selects `local-mac`).

### Paths

| Path | What |
|------|------|
| `~/actions-runner-<name>` | the runner install + its `.env` |
| `~/actions-runner-<name>/_toolcache/{rustup,cargo}` | runner-private Rust toolchain state |
| `<ci-root>/work/<name>` | that runner's `_work` (isolated per runner) |
| `<ci-root>/cache/ccache` | shared ccache (size set by host `ccache.conf`) |
| `<ci-root>/cache/fetchcontent-src` | shared CMake FetchContent sources |

`--dry-run` prints the full plan (names, work dirs, labels, parallelism)
without downloading, configuring, or starting anything.

## List

```bash
shipyard runner list                       # repos discovered from local dirs + cwd
shipyard runner list --repo Generous-Corp/pulp [--repo …] [--all-repos]
```

Renders the live pool grouped by machine tag, pulled straight from GitHub (so a
laptop's runners show up even when you run it on the Studio). It also scans this
machine's `~/actions-runner-*` dirs and flags **orphans** — local runner dirs
whose configured name is no longer registered on GitHub (e.g. a deregistered
runner whose directory lingers).

## Remove

```bash
shipyard runner remove --name pulp-studio-03 --yes [--purge-dir]
```

Stops the LaunchAgent, fetches a removal token, and runs `config.sh remove`.
`--purge-dir` also deletes `~/actions-runner-<name>`. Requires `--yes`.

## Adding a brand-new machine (e.g. an M5 laptop)

1. Provision the host for the repo (its own bootstrap: Xcode, Homebrew deps,
   Skia, ccache). For a fresh python.org Python, run its bundled
   `Install Certificates.command` first or asset downloads fail with
   `SSL: CERTIFICATE_VERIFY_FAILED`.
2. `shipyard runner tag --set m5`
3. `shipyard runner register --repo <owner/repo> --count <N> --ci-root <dir>`
4. `shipyard runner list --repo <owner/repo>` to confirm `<repo>-m5-NN` online.

The index continues from any existing runners on other machines, so the M5's
runners never collide with the Studio's or the laptop's.
