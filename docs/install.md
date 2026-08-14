# Installation

## Canonical install location

Every supported install path writes the `shipyard` binary to the same
place by default:

```
~/.local/bin/shipyard
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

Downloads the right binary for your platform and installs it at
`~/.local/bin/shipyard`.

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

### GitHub App / bot identity

If Shipyard is running under an App install (CI, `RELEASE_BOT_TOKEN`,
a bot like `pulp-release-bot`), the scope lives on the **App's
permissions**, not the invoking user's token. github.com →
organizations/<org> → Settings → GitHub Apps → your app →
**Permissions & events** → **Actions: Read and write**. Accept the
installation permission update after saving.

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
```

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
| Repository permissions | `Contents: Read-only`; add `Actions`, `Checks`, `Commit statuses`, and `Pull requests` as read-only for fuller Shipyard inspection |
| Organization permissions | `Self-hosted runners: Read-only` when inspecting or verifying organization runner groups; Read & write only for configuration/removal |
| Subscribe to events | none for quota testing |
| Installable by | Only on this account |

After creating the app, install it on the account and choose `All repositories`
when validating the scaled installation bucket. Save the App ID, installation
ID, and private-key path locally; never put the private key in tracked config.

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

# Optional. If absent, the helper uses --repo owner/name to look up the
# installation id for that repository.
export SHIPYARD_GITHUB_APP_INSTALLATION_ID=987654
```

Shipyard only runs the helper, reads stdout, caches the returned token in
memory until expiry, and injects it into child `gh` commands.

Preferred helper stdout for expiring tokens:

```json
{
  "token": "ghs_...",
  "expires_at": "2026-05-26T20:12:00Z",
  "kind": "github-app-installation"
}
```

Plain token stdout is also accepted. Plain tokens are cached only when
`cache_ttl_seconds` is set.

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
var names, helper command names, and notes. It does not include tokens,
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

| OS | Architecture | Binary |
|----|-------------|--------|
| macOS | Apple Silicon (ARM64) | `shipyard-macos-arm64.dmg` |
| Windows | x64 | `shipyard-windows-x64.exe` |
| Linux | x64 | `shipyard-linux-x64` |
| Linux | ARM64 | `shipyard-linux-arm64` |

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
ln -sf ~/.local/bin/shipyard ~/.local/bin/sy
```

Only do this intentionally: Claude Code, Codex, the macOS GUI, and
project pinners all treat `~/.local/bin/shipyard` as canonical.

## Optional dependencies

You don't need everything — just what matches your setup. See the
[main README requirements table](../README.md#requirements) for
details.
