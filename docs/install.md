# Installation

## Canonical install location

Every supported install path writes the `shipyard` CLI and its provider
companion to the same directory by default:

```
~/.local/bin/shipyard
~/.local/bin/shipyard-workstream-provider
```

| Path | Lands at |
|---|---|
| `curl … install.sh` | `~/.local/bin/shipyard` |
| Claude Code plugin (auto-installs on first session if missing) | `~/.local/bin/shipyard` |
| Codex one-liner (same `install.sh`) | `~/.local/bin/shipyard` |
| Project pinners (see "Pin a specific version" below) | `~/.local/bin/shipyard` (recommended) |

Make sure `~/.local/bin` is on your `PATH` and every install method
reaches the same binary. No PATH juggling, no "which shipyard am I
running" confusion.

## Quick install

```bash
curl -fsSL https://generouscorp.com/Shipyard/install.sh | sh
```

Downloads the matched CLI/provider pair for your platform and installs both in
`~/.local/bin`. A rollback to a release before v0.127.0 removes the newer
provider after the older CLI passes its smoke test.

## First-run auth

A few Shipyard commands (`cloud retarget`, `cloud handoff`, anything
that cancels + re-dispatches a GitHub Actions workflow run) need a
`gh` token with the **`workflow` scope** — GitHub's short name for
`actions:write` on a classic PAT, or **Actions: Read and write** on a
fine-grained token. Without it you'll hit a classified cancellation
failure before Shipyard dispatches a replacement:

```
Couldn't cancel every matching job for PR #224 target=mac; no replacement dispatch was sent.
Cancellation failed for job 71460714958 (macOS (ARM64) [github-hosted]): scope HTTP 403.
Auth recovery: run `gh auth refresh -h github.com -s workflow`, or grant Actions: Read and write on the token/App identity.
```

`shipyard doctor` probes for this; fix it at install time so the
first retarget attempt doesn't surprise you. If the classification is
`not_found` instead of `scope`, the failure usually means GitHub could
not see or cancel that job anymore; open the run URL Shipyard prints,
cancel manually if needed, then re-run the retarget command.

### Interactive gh login (most common)

```bash
gh auth refresh -h github.com -s workflow
```

Follow the browser prompt. You don't have to log out first —
`refresh` adds the scope to your existing session.

### Fine-grained personal access token

github.com → Settings → Developer settings → Personal access tokens →
**Fine-grained tokens** → edit the token that's stored in `gh auth` →
**Actions: Read and write**. Save. `gh auth status` should now show
the scope in its `Token scopes:` line.

If the same token is used for repository runner inventory or recovery, also
grant **Administration: Read-only**. Use **Read and write** only when the
token holder must generate or remove runner registrations. This includes
interactive `shipyard runner register` and `shipyard runner remove` calls.
`Actions` permission
does not cover the repository `/actions/runners` endpoints.

### GitHub App / bot identity

If Shipyard is running under an App install (CI, `RELEASE_BOT_TOKEN`,
a bot like `pulp-release-bot`), the scope lives on the **App's
permissions**, not the invoking user's token. github.com →
organizations/<org> → Settings → GitHub Apps → your app →
**Permissions & events** → **Actions: Read and write**. Accept the
installation permission update after saving.

If Shipyard also inspects repository runner inventory (fleet admission or
stale-runner recovery proof), grant **Repository permissions →
Administration: Read-only**. GitHub places the repository
`/actions/runners` endpoints under Administration, not Actions. This read lets
Shipyard observe registered runners and their online/busy state, and fail
closed when the inventory cannot be read; it does not authorize deletion. Use
**Read and write** whenever the App-backed Shipyard operation must generate
registrations or reclaim a proven-stale runner, whether interactive or
unattended.

If the Shipyard deployment includes an external organization runner-group
verifier, also grant
**Organization permissions → Self-hosted runners: Read-only**. Repository
`Actions` access does not cover the organization runner-group API. Use Read &
write only when Shipyard must configure runner groups or remove registrations.
After any App permission change, approve the installation update and mint a
fresh installation token; an existing cached token retains its old permissions.

## Optional Shipyard GitHub auth override

By default, Shipyard uses the same ambient `gh` login as before. You do
not need this section unless you want Shipyard's built-in GitHub calls
to use a different quota bucket or a more portable credential setup.
If `GH_TOKEN` is exported in the parent environment, `gh` itself gives
that token precedence over keychain auth.

Add `[github.auth]` to global config, `.shipyard/config.toml`, or
`.shipyard.local/config.toml`. Prefer local config for personal helper
paths and vault names. Shipyard never stores raw tokens, GitHub App
private keys, Keychain items, or 1Password sessions.

Environment token:

```toml
[github.auth]
source = "env"
token_env = "SHIPYARD_GITHUB_TOKEN"
```

Set `SHIPYARD_GITHUB_TOKEN` on each Mac through your shell, direnv,
launch agent, or secret manager. Shipyard injects it only into child
`gh` commands as `GH_TOKEN`.

macOS Keychain helper:

```toml
[github.auth]
source = "command"
token_command = ["security", "find-generic-password", "-w", "-s", "shipyard-github-token"]
cache_ttl_seconds = 300
```

1Password helper:

```toml
[github.auth]
source = "command"
token_command = ["op", "read", "op://Private/shipyard/github-token"]
cache_ttl_seconds = 300
```

GitHub App installation helper:

```toml
[github.auth]
source = "command"
token_command = ["/Users/you/Code/shipyard/scripts/shipyard-github-app-token", "--repo", "{repo_slug}"]
refresh_skew_seconds = 60
privileged_gh_binary = "/opt/homebrew/bin/gh"
privileged_git_binary = "/usr/bin/git"
```

The privileged binary paths are required for dependency pin qualification and
publication. Put them only in trusted machine-global config. Both must be
absolute native executables; Shipyard will not discover a token-bearing `gh` or
`git` through `PATH`.

Some low-volume mutations cannot be performed by a GitHub App installation
token. When GitHub returns the exact integration-permission denial for one of
those documented fallbacks, Shipyard removes both `GH_TOKEN` and
`GITHUB_TOKEN` and invokes a direct native GitHub CLI so its keyring login can
be used. Script and wrapper candidates named `gh` are skipped, preventing an
ambient fallback from routing back through an App-token wrapper. Shipyard scans
`PATH` for the first native `gh` by default; machines that need an explicit
location can pin it in the same config:

```toml
[github.auth]
source = "command"
token_command = ["/absolute/path/to/shipyard-github-app-token", "--repo", "{repo_slug}"]
ambient_gh_binary = "/absolute/path/to/gh"
```

`ambient_gh_binary` must be absolute and resolve to an executable native
binary, not a shell script or wrapper. It is a per-machine path: update it when
importing an auth bundle onto a machine whose GitHub CLI is installed
elsewhere.

For the full quota-extension walkthrough, including GitHub App registration
fields, repository-count scaling, and validation commands, see
[`docs/github-app-quota.md`](github-app-quota.md).

For GitHub Apps, registration and installation are still manual. Register the
app under the personal account or organization that will own it, install it on
the account whose repositories Shipyard should inspect, and create a private
key. The repositories themselves do not need to be GitHub Apps; the installation
just needs access to them.

Minimal GitHub App registration for a local Shipyard quota/auth helper:

| Field | Value |
|---|---|
| GitHub App name | A private name such as `shipyard-local` |
| Homepage URL | The Shipyard repo URL or the owning account URL |
| Callback URL | blank |
| Request user authorization / OAuth / Device Flow | disabled |
| Setup URL / Redirect on update | blank / disabled |
| Webhook Active | disabled |
| Repository permissions | `Contents: Read-only`; add `Actions`, `Checks`, `Commit statuses`, and `Pull requests` as read-only for fuller Shipyard inspection; add `Administration: Read-only` for repository runner inventory |
| Organization permissions | `Self-hosted runners: Read-only` when inspecting or verifying organization runner groups; Read & write only for configuration/removal |
| Subscribe to events | none for quota testing |
| Installable by | Only on this account |

After creating the app, install it on the account and choose `All repositories`
when validating the scaled installation bucket. Save the App ID, installation
ID, and private-key path locally; never put the private key in tracked config.

`[github.auth]` selects how Shipyard obtains a token; it cannot widen that
token's GitHub App permissions. If runner inspection returns `Resource not
accessible by integration`, update the App installation permissions above and
mint a fresh installation token rather than falling back to an unrelated
ambient credential.

### Unattended self-update and fleet rollout

`shipyard update` reads GitHub auth only from the trusted machine-global
configuration returned by `shipyard paths`. When that file declares an env or
command source, helper failure stops before downloading or replacing anything;
Shipyard does not silently fall back to the anonymous GitHub API. The updater
downloads the installer from the exact target tag completely before executing
it, and `--refresh-daemon` restarts the daemon only after the installer's staged
binary smoke passes. The verified newly installed binary performs the refresh
with the caller's exact mode, global-config directory, and state directory; the
older updater process never reuses its own daemon-spawn implementation after
replacement. A non-zero child exit or malformed refresh receipt fails closed.

Non-login SSH and launchd do not need a caller-built `PATH`. Shipyard resolves
system curl/Bash from canonical absolute locations and starts detached daemons
with Homebrew, `/usr/local/bin`, `~/.local/bin`, and system tool directories in
a deterministic PATH. The daemon retains its state-owned private `TMPDIR`, but
local validation subprocesses that inherit that protected path receive a fresh
owner-private directory under the platform's real temporary root. This keeps
test fixtures ephemeral and outside the production writer domain without
weakening protected-path classification. Custom locations can be configured
globally:

```toml
[update]
curl_bin = "/usr/bin/curl"
shell_bin = "/bin/bash"
```

For one exact fleet rollout, give every remote host class the installed
Shipyard path (the local/controller class may omit it):

```toml
[host_class.m5]
ssh = "m5-lan"
shipyard_bin = "/Users/you/.local/bin/shipyard"
github_cli = "/Users/you/.local/bin/ghapp"
github_token_helper = "/Users/you/.config/shipyard/bin/shipyard-github-app-token"
shipyard_mode = "shipyard"
shipyard_global_dir = "/Users/you/Library/Application Support/shipyard"
shipyard_state_dir = "/Users/you/Library/Application Support/shipyard"
```

Then review and apply the same immutable release plan:

```sh
shipyard runner fleet-update --to vX.Y.Z --host-class m5 --json
shipyard runner fleet-update --to vX.Y.Z --host-class m5 --apply --json
```

Repeat `--host-class` for an intentionally ordered subset or use explicit
`--all-hosts`. Omission, unknown names, and duplicates fail closed, and apply
stops before every later host after the first failure.

The configured `github_cli` must be the `ghapp` sibling of `shipyard_bin`.
Its machine-global `[github.auth]` must use the exact eight-element wrapper
command documented in [GitHub App quota and routing](github-app-quota.md): one
wrapper, `token`, `--app-id VALUE`, `--private-key ABSOLUTE_PATH`, and literal
`--repo {repo_slug}`. Direct `ghapp` commands resolve only that machine-global
credential shape through the sibling Shipyard binary; repository overlays,
fixed installation IDs, API/cache arguments, and foreign wrappers fail before
the token helper runs. Direct mode requires a strict 0600 typed
`ghapp.shipyard-context.json` sibling; fleet writes it for targets v0.131.0 and
newer so subsequent direct wrapper calls retain the configured runtime mode and
global directory. Manual/non-fleet installs must provision the same context
beside `ghapp`; missing or unsafe context fails closed.

The command uses a stripped remote environment deliberately, invokes the
configured absolute binary, bounds each host attempt to ten minutes, and
refreshes each host's daemon only after its update succeeds. Because inherited
secrets are intentionally stripped, fleet rollout requires a machine-global
`github.auth.source = "command"` helper that can resolve credentials from its
own durable configuration using only `HOME` and the canonical PATH. Remote
classes also use their absolute `github_cli` helper to download and run the
exact tagged installer directly. The one-time transition from v0.130.x or
older to a resolver-capable v0.131.0-or-newer target first requires an ordinary
`shipyard update --to vX.Y.Z --refresh-daemon` on each host using its existing
machine credential, followed by migration to the exact wrapper command above.
The governed fleet update then verifies and commits the complete paired state;
without that predeployment it refuses before download or mutation. The
explicit mode/config/state fields bind verification and refresh to the daemon
the profile actually owns; direct
`shipyard update` continues to support env auth. A missing absolute profile
path is reported as launch-
environment drift, not as evidence that Homebrew, Tart, or Shipyard is
uninstalled. Fleet rollout rejects targets older than v0.100.0 before mutation;
use an older release's documented manual procedure when rollback crosses that
bootstrap boundary.

For targets v0.131.0 and newer, fleet installation commits its helper, wrapper,
resolver context, Shipyard binary, and companion
transaction only after the newly installed Shipyard resolves the installed
wrapper using the host class's exact mode and global directory. A resolver
failure restores all five prior artifacts before the host can report success.
Targets v0.100.0 through v0.130.x preserve the legacy four-target transaction
and nine-line recovery journal, with no resolver context or unsupported probe.

Before any host can mutate, fleet rollout resolves the annotated release tag to
its full tag-object, commit, and tree OIDs; binds the published release ID; and
downloads the exact `checksums.sha256` and `shipyard-macos-arm64.dmg` assets by
asset ID. Both downloads must match GitHub's SHA-256 metadata, the manifest must
contain exactly one matching DMG entry, and `gh attestation verify` must bind
the DMG to `danielraffel/Shipyard/.github/workflows/release.yml`, the exact tag
ref, and source commit. A missing DMG attestation makes the release ineligible;
an operator-provided tag or receipt cannot substitute for this verification.

Each successful JSON host receipt carries that complete immutable authority and
before/after primary and adjacent `shipyard-workstream-provider` paths,
versions, and double-observed SHA-256 values. Pre-install source provenance is
explicitly unverified; post-install source identity is the canonical digest of
the verified release authority. Paired releases from v0.127.0 onward must match
exactly, while a legacy rollback must prove the companion absent. The verifier
closes its mint window by re-reading the tag, release ID, and asset inventory,
then freezes one authority for the rollout. Every host must return that exact
authority digest and platform-asset digest; receipts are never reminted from
mutable GitHub state between hosts. Cross-host installed pair hashes must also
agree, and any mismatch stops every later host.

Runner-group access is valuable when Shipyard coordinates multiple local
providers because an App-backed policy verifier can compare selected
repositories, selected workflows, and live group membership before the
deployment treats capacity as trusted. This verifier is an operator integration,
not currently a built-in `shipyard runner` check. It keeps fleet
coordination separate from execution—TartCI can own disposable Apple-Silicon
macOS VMs, Proxmox can own x64 Linux VMs, and native Intel hardware can run
macOS/Metal checks—without broadening any one runner's authorization.

`scripts/shipyard-github-app-token` is a zero-Python-dependency helper for this
flow. It uses `openssl` to sign the app JWT, asks GitHub for an installation
access token, and prints the JSON shape Shipyard expects. Configure it with
flags or environment variables:

Use an absolute helper path in personal/local config when you plan to export
and import auth settings into other repositories. A repo-relative helper such as
`scripts/shipyard-github-app-token` only works from the Shipyard checkout.

```bash
export SHIPYARD_GITHUB_APP_ID=123456
export SHIPYARD_GITHUB_APP_PRIVATE_KEY_PATH="$HOME/.config/shipyard/github-app.pem"

# Repo-less compatibility only. Do not set this for a helper shared across
# personal and organization installations; --repo is authoritative.
export SHIPYARD_GITHUB_APP_INSTALLATION_ID=987654
```

Shipyard only runs the helper, reads stdout, caches the returned token in
memory until expiry, and injects it into child `gh` commands. Cache entries are
partitioned by the fully expanded helper argv, so `{repo_slug}` keeps tokens for
different repositories/installations separate.

For one App installed on multiple accounts, always keep `--repo
"{repo_slug}"` in `token_command` and remove any fixed installation id from the
shared wrapper. The helper asks GitHub for the installation attached to that
exact repository. A legacy `SHIPYARD_GITHUB_APP_INSTALLATION_ID` is ignored
when `--repo` is present; an explicit `--installation-id` alongside `--repo`
becomes a fail-closed assertion and must match GitHub's lookup.

The optional `--cache-dir` (or `SHIPYARD_GITHUB_APP_CACHE_DIR`) stores one
expiry-checked entry per API host, App, and repository/installation. The helper
requires the directory to be `0700` and every token entry to be `0600`, writes
atomically, and refuses entries that are malformed, aliased, owned by another
account, or provenance-mismatched. The App key likewise must be a current-user
owned `0600` regular file inside a current-user owned `0700` directory. Leave
the disk cache unset when Shipyard's own in-memory cache is sufficient.
On Unix/macOS, a cache under a real Shipyard protected root joins the same fair
writer domain as the Rust CLI. Reads and GitHub requests remain outside the
lease; directory creation and atomic replacement wait up to 30 seconds for a
Sandbox E2E exclusive audit, then fail without writing using exit `75` and the
stable `sandbox_writer_domain_overlap` classification.
On Windows, disk caching currently fails closed because this helper does not yet
prove private Windows ACLs; leave `--cache-dir` and its environment variable
unset there. Non-cache token minting remains available. This is a bounded
confidentiality restriction, not a promise of POSIX-mode emulation on Windows.

The helper resolves `openssl` only from trusted absolute platform paths. Set
`SHIPYARD_GITHUB_APP_OPENSSL` to another absolute executable only when the host
does not provide one of those paths; never rely on a repository-controlled
`PATH` for a process that can read the App key.

Preferred helper stdout for expiring tokens:

```json
{
  "token": "ghs_...",
  "expires_at": "2026-05-26T20:12:00Z",
  "kind": "github-app-installation"
}
```

Plain token stdout is also accepted. Plain tokens are cached only when
`cache_ttl_seconds` is set. A plain token with GitHub's documented `ghs_`
installation-token prefix is reported as `kind=github-app-installation`, so
App-only commands can preserve their authority boundary with existing helpers.

Helpers must write tokens only to stdout. Shipyard redacts common GitHub
token prefixes in diagnostics, but helper stderr is still surfaced for
debugging and should not contain secrets. The optional `kind` field
should be a stable label such as `github-app-installation`, not
free-form sensitive text.

Supported placeholders in `token_command`:

| Placeholder | Expands to |
|---|---|
| `{repo_slug}` | `owner/repo` from `origin` |
| `{repo_owner}` | repo owner |
| `{repo_name}` | repo name |
| `{cwd}` | current working directory |

Run `shipyard doctor --rate-limit` to confirm which auth source is in
use and which REST/GraphQL buckets it sees. This actively resolves the
configured token source, so command helpers may run and App helpers may
mint an installation token. For env, command, and App tokens, classic
scopes may not be locally inspectable with `gh auth status`; verify
Actions permissions in GitHub when using cloud retarget or handoff.

Focused auth commands:

```bash
shipyard auth doctor
shipyard auth export --output shipyard-auth.toml
shipyard auth import shipyard-auth.toml --scope local
```

`auth export` writes a config-only bundle: `[github.auth]`, required env
var names, helper and ambient-CLI paths, and notes. It does not include tokens,
private keys, Keychain items, 1Password sessions, queue state, daemon
sockets, or token caches. `auth import` writes only the `[github.auth]`
section into the selected config layer: `local` (default), `project`, or
`global`.

### Moving credentials to another Mac

The signed Shipyard binary and `.dmg` are credential-free and portable.
Move non-secret config, then reprovision the credential outside
Shipyard on the destination Mac:

1. Copy global, tracked, or local Shipyard config as appropriate.
2. Recreate the env var, Keychain item, 1Password sign-in, or App helper.
3. If using a GitHub App, provision a private key or secret-manager
   reference for that Mac.
4. Run `shipyard doctor --rate-limit`.

Do not copy Shipyard queue state, daemon sockets, local runner state,
`gh auth` keychain state, private keys, or token caches between Macs.

## Pin a specific version

Pass `SHIPYARD_VERSION` to install an exact release instead of the
latest. Useful for project-pinning so every teammate + agent runs
the same shipyard build.

```bash
SHIPYARD_VERSION="v0.22.1" curl -fsSL https://generouscorp.com/Shipyard/install.sh | bash
# or if you've already fetched the script:
SHIPYARD_VERSION="v0.22.1" bash install.sh
```

Accepts `"v0.22.1"`, `"0.22.1"`, or `"latest"` (default).

Project-level pinning pattern: keep the desired version in a small
pin file (e.g. `tools/shipyard.toml` with `version = "0.22.1"`), read
it in a wrapper script, and call `install.sh` with
`SHIPYARD_VERSION="$(read-version)"`. Nothing more complicated is
needed — every teammate ends up with the same binary at
`~/.local/bin/shipyard`.

## Install to a different directory

Pass `SHIPYARD_INSTALL_DIR`. Only override when you have a specific
reason; the default keeps every install path aligned.

```bash
SHIPYARD_INSTALL_DIR="${HOME}/mytools/bin" bash install.sh
```

## Platform binaries

| OS | Architecture | Release assets |
|----|-------------|--------|
| macOS | Apple Silicon (ARM64) | `shipyard-macos-arm64.dmg` (contains both binaries) |
| Windows | x64 | `shipyard-windows-x64.exe`, `shipyard-workstream-provider-windows-x64.exe` |
| Linux | x64 | `shipyard-linux-x64`, `shipyard-workstream-provider-linux-x64` |
| Linux | ARM64 | `shipyard-linux-arm64`, `shipyard-workstream-provider-linux-arm64` |

Intel Macs (x86_64) are not supported from v0.50.0 onward. Apple Silicon only. Older releases (v0.44.0–v0.49.0) that shipped Intel dmgs remain installable by pinning `SHIPYARD_VERSION`; `install.sh` on an Intel Mac surfaces a clear "unsupported" message instead of a 404 on v0.50.0+.

## Build from source

### Isolated dev build

Your dev build lives in the checkout under `target/`; the system
`shipyard` at `~/.local/bin/shipyard` is unaffected unless you copy or
install it there.

```bash
git clone https://github.com/danielraffel/Shipyard.git
cd Shipyard
cargo build --release --locked
target/release/shipyard --version
target/release/shipyard-workstream-provider --version
```

Run the main local gates before relying on a source build:

```bash
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
python3 -m unittest discover -s scripts -p 'test_*.py'
```

### Make a source build your system `shipyard`

If you want your local checkout to take over at
`~/.local/bin/shipyard` (same location `install.sh` uses), copy the
release binary and refresh the `sy` symlink:

```bash
mkdir -p ~/.local/bin
cp target/release/shipyard ~/.local/bin/shipyard
cp target/release/shipyard-workstream-provider ~/.local/bin/shipyard-workstream-provider
ln -sf ~/.local/bin/shipyard ~/.local/bin/sy
```

Only do this intentionally: Claude Code, Codex, the macOS GUI, and
project pinners all treat `~/.local/bin/shipyard` as canonical.

## Optional dependencies

You don't need everything — just what matches your setup. See the
[main README requirements table](../README.md#requirements) for
details.
