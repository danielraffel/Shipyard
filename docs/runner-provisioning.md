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
   appends exactly `--count` capacity (`-04`, `-05`) without collisions. Existing
   configured runners on this host are also included as a separate upgrade set;
   they do not consume the additive count. Every existing runner retains its
   allocation from `.env`; Shipyard also discovers configured local runners for
   other repositories and older machine tags, reserves all those slots, then divides
   remaining host cores across the additive runners. It exits 3 instead of
   adding capacity when existing allocations leave fewer than one core per new
   runner. Preserving existing allocations keeps a runner that becomes active
   after preflight from invalidating the capacity plan.
2. Downloads the fleet-pinned `osx-arm64` runner tarball once into
   `<ci-root>/cache/actions-runner-pkg/` and extracts it into
   `~/actions-runner-<name>`. Both the runner archive and pinned `rustup-init`
   binary are SHA-256 verified. Registration uses `--disableupdate`; upgrades
   are an explicit fleet-wide pin change, never a per-host automatic update.
   Before touching an existing installation, Shipyard parses `.runner` and
   requires both `agentName` and the GitHub repository URL to match the planned
   runner and requested repo. Live GitHub `status`/`busy` evidence is retained in
   the plan. Shipyard never automatically stops or upgrades a service-installed
   runner. An already-pinned service is retained unchanged; a service that
   needs an upgrade is reported as deferred and causes exit 3, leaving its
   current job eligibility unchanged. A configured service-less runner is upgradeable
   only after fresh GitHub observations prove it is offline and idle both
   immediately before clone-staging and at the activation boundary; online,
   busy, missing, or unknown evidence fails closed. Eligible upgrades
   clone-stage the complete installation, extract and verify the new runner,
   and prepare its toolchain before an atomic rename. The intact old directory
   is retained until the replacement service starts; a partially started
   replacement is stopped and removed through the runner's compound
   `svc.sh uninstall` before the verified original directory is restored. A failed
   extraction never modifies the live directory. `config.sh` and `svc.sh
   install` are only for genuinely new or service-less configured runners.
   A runner that becomes busy, online, or service-managed during staging is
   deferred without blocking the separately requested additive runners.
   Readiness probes
   suppress child stdout and stderr so both human and JSON command output remain
   machine-readable.
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

Uses the runner's compound `svc.sh uninstall` to stop the service and remove
its LaunchAgent registration, then fetches a removal token and runs
`config.sh remove`. A failed service uninstall fails closed before GitHub
deregistration so a stale `runsvc.sh` Background Item is not silently left
behind.
`--purge-dir` also deletes `~/actions-runner-<name>`. Requires `--yes`.

## Adding a brand-new machine (e.g. an M5 laptop)

1. Provision the host for the repo (its own bootstrap: Xcode, Homebrew deps,
   Skia, ccache). For a fresh python.org Python, run its bundled
   `Install Certificates.command` first or asset downloads fail with
   `SSL: CERTIFICATE_VERIFY_FAILED`.
2. `shipyard runner tag --set m5`
3. `shipyard runner register --repo <owner/repo> --count <N> --ci-root <dir>`
4. `shipyard runner list --repo <owner/repo>` to confirm `<repo>-m5-NN` online.

## GitHub TLS/DNS diagnostic on Mac hosts

If `ghapp` or Shipyard reports a TLS handshake timeout while GitHub Status is
operational, first test the host rather than treating the event as a GitHub
outage. Run the same read-only probe in both resolver modes:

```bash
python3 - <<'PY'
import os, subprocess

for label, extra in (("default", {}), ("go", {"GODEBUG": "netdns=go"})):
    env = os.environ.copy()
    env.update(extra)
    try:
        result = subprocess.run(
            ["ghapp", "api", "repos/OWNER/REPO/git/ref/heads/main", "--jq", ".object.sha"],
            env=env, text=True, capture_output=True, timeout=8,
        )
        print(label, result.returncode, (result.stdout or result.stderr).strip())
    except subprocess.TimeoutExpired:
        print(label, "timeout")
PY
```

Record the host, macOS build, resolver mode, elapsed time, and SHA/error. A
`default` timeout with a successful `GODEBUG=netdns=go` probe identifies a
host-local Go/cgo DNS/TLS path problem. Do not apply that workaround fleet-wide
without reproducing it: the M1 and M5 probes on 2026-08-12 succeeded in both
modes, while the controller Mac Studio reproduced the difference.

Do not stop after proving `api.github.com` is reachable when the failing
operation downloads an Actions job log. GitHub answers the authenticated job
log request with a redirect to an Azure Blob hostname, and that second host can
have a different DNS/TLS failure. Inspect the redirect and probe both hosts:

```bash
ghapp api -i repos/OWNER/REPO/actions/jobs/JOB_ID/logs
curl -4 -I --connect-timeout 8 https://api.github.com
curl -4 -I --connect-timeout 8 https://REDIRECTED-HOST
```

If the API host succeeds but the redirected host times out, preserve the job
ID and fetch the log from another healthy managed host using that host's own
`ghapp` authentication. Do not copy GitHub App tokens or other credentials
between machines. Record both hostnames and resolver answers so the incident
is not misclassified as a missing permission or an empty log.

GitHub Actions runner proxy variables must name an HTTP or HTTPS proxy. Do not
put a `socks5://` URL in `HTTP_PROXY` or `HTTPS_PROXY`: the runner may connect,
but Node-based actions such as `actions/checkout@v5` reject that URL before
checkout. If this host needs its SOCKS tunnel, expose it through a host-local
HTTP CONNECT adapter and give the runner that adapter's `http://127.0.0.1:PORT`
URL. Before rerunning a failed check, prove the idle listener inherited the new
scheme (without printing unrelated environment values):

```bash
pgrep -f 'Runner.Listener' | while read -r pid; do
  ps eww -p "$pid" | tr ' ' '\n' |
    grep -Ei '^(http|https|no)_proxy='
done
```

Restart an idle runner through its service command after changing the local
proxy adapter or service environment; never kill a busy runner process.

For Git transport failures, test SSH over GitHub's HTTPS port and verify the
published GitHub Ed25519 fingerprint before accepting the host key:

```bash
ssh-keyscan -p 443 -t ed25519 ssh.github.com | ssh-keygen -lf -
ssh -T -p 443 -o HostName=ssh.github.com git@ssh.github.com
```

If the 1Password SSH agent stalls during signing, bypass only that agent for
the operation and select an authorized local key explicitly:

```bash
GIT_SSH_COMMAND='ssh -o HostName=ssh.github.com -p 443 \
  -o IdentityAgent=none -o IdentitiesOnly=yes -i ~/.ssh/id_rsa' \
  git fetch origin
```

Keep `GODEBUG=netdns=go` and the SSH-over-443 setting host-local (bootstrap,
LaunchAgent environment, or the local `ghapp`/Shipyard wrapper). Do not put
machine-network workarounds in a repository's `CLAUDE.md`, source, or public
skills. Re-test after OS, Go, Shipyard, DNS, or 1Password-agent updates.

The index continues from any existing runners on other machines, so the M5's
runners never collide with the Studio's or the laptop's.
