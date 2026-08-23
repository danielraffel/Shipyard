# GitHub App quota for Shipyard

Shipyard can use a GitHub App installation token instead of your ambient
`gh` login. This is useful when Shipyard needs to inspect or coordinate many
repositories and your normal authenticated user bucket of 5,000 REST API
requests per hour is too small.

The repositories do not need to be GitHub Apps. The app is just the credential
Shipyard uses. Install the app on the personal account or organization that owns
the repositories, grant the app access to those repositories, then configure
Shipyard to mint installation access tokens through a helper command.

## What limit should I expect?

GitHub documents GitHub App installation tokens as their own rate-limit bucket:

- Minimum installation-token limit: 5,000 REST requests per hour.
- GitHub Enterprise Cloud organization installation limit: 15,000 requests per
  hour.
- Non-Enterprise installations scale with repository count and, for
  organizations, user count.
- Installations with more than 20 repositories receive 50 additional requests
  per hour for each repository above 20.
- The scaled non-Enterprise limit caps at 12,500 requests per hour.

For repository-count scaling, the practical formula is:

```text
min(12,500, 5,000 + max(0, repo_count - 20) * 50)
```

That means an installation reaches the 12,500/hour cap at 170 repositories:

```text
5,000 + (170 - 20) * 50 = 12,500
```

If your personal account has more than 200 repositories and the GitHub App
installation is granted access to those repositories, it should qualify for the
12,500/hour installation-token bucket. GitHub Pro does not materially change
this calculation; the important part is that Shipyard uses a GitHub App
installation access token, not a personal access token and not a GitHub App user
access token.

GitHub's rate-limit docs are the source of truth:

- REST API rate limits:
  <https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api>
- Registering a GitHub App:
  <https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/registering-a-github-app>

## Caveats

Secondary rate limits still apply. A larger primary bucket does not let Shipyard
make unlimited concurrent requests, hammer one endpoint, or create content too
quickly.

The higher bucket applies only to GitHub App installation access tokens. GitHub
App user access tokens are counted against the authenticated user's normal rate
limit bucket, just like PAT-backed user automation.

The installation can only query repositories it can access. If you install the
app on selected repositories, Shipyard only gets the scaled bucket for that
installation and can only inspect those selected repositories. For quota
validation across a large personal account, install on all repositories.

Private and public repositories can both be included, but permissions still
matter. Keep permissions minimal for read-only inspection. Add write permissions
only for workflows that need them, such as cancelling or dispatching workflow
runs.

## Register the GitHub App

Create the app from GitHub:

```text
GitHub -> Settings -> Developer settings -> GitHub Apps -> New GitHub App
```

For a local private Shipyard quota helper, these settings are enough:

| Field | Value |
|---|---|
| GitHub App name | A private name, for example `shipyard-local` |
| Homepage URL | The Shipyard repo URL or your GitHub profile URL |
| Callback URL | Blank |
| Expire user authorization tokens | Leave enabled; ignored if you do not use user tokens |
| Request user authorization during installation | Disabled |
| Enable Device Flow | Disabled |
| Setup URL | Blank |
| Redirect on update | Disabled |
| Webhook Active | Disabled for quota-only local use |
| Repository permissions | Start with `Contents: Read-only`; add more only as needed |
| Subscribe to events | None for quota-only local use |
| Where can this GitHub App be installed? | `Only on this account` for a private helper |

For broader Shipyard inspection, common read-only repository permissions are:

| Permission | Suggested level |
|---|---|
| Contents | Read-only |
| Actions | Read-only, or Read & write if Shipyard must cancel/dispatch workflows |
| Administration | Read-only for repository runner inventory; Read & write whenever App-backed Shipyard must mint or remove runner registrations, including interactive `shipyard runner register/remove` |
| Checks | Read-only |
| Commit statuses | Read & write when using steward handoff/recovery; otherwise read-only |
| Issues | Read & write when using steward ownership/recovery labels |
| Pull requests | Read & write for queue stewardship; otherwise read-only |
| Metadata | Always available |

`Actions` and `Administration` are intentionally separate here. Workflow/run
inspection uses Actions, while GitHub's repository runner endpoints
(`/repos/{owner}/{repo}/actions/runners`) use Administration. Shipyard fleet
admission and recovery code reads those endpoints to bind an
observation to the configured runner and inspect its online/busy state before
it acts. Without runner read access, the safe result is unknown/defer, not
“idle” and not a broad reset. Read-only is sufficient for inspection and stale proof;
write is justified only for explicit registration mint/reclaim operations.

Organization runner-group inspection is a separate permission surface:

| Permission | Suggested level |
|---|---|
| Self-hosted runners | Read-only when an operator integration lists or verifies organization runner-group policy; Read & write only when it configures groups or removes registrations |

Repository `Actions` permission does not grant access to
`/orgs/<org>/actions/runner-groups/...`. Shipyard does not currently perform this
organization-policy comparison itself. The permission lets an App-backed
verifier deployed alongside Shipyard fail closed when a trusted group's selected
repositories, selected workflows, or live runners drift from policy. It enables one control plane to
coordinate heterogeneous capacity without confusing the providers: for example,
TartCI-managed macOS VMs on Apple Silicon, a separate Proxmox x64 Linux pool,
and native Intel macOS/Metal hardware.

If a read-oriented App rejects a low-volume steward status or label mutation
with GitHub's exact `Resource not accessible by integration` response,
Shipyard prints an explicit warning and retries that mutation with ambient
`gh` credentials only. Unattended controllers should grant the App (or their
workflow `GITHUB_TOKEN`) the write permissions above so they never depend on
an interactive user's ambient login.

After creating the app:

1. Generate a private key from the app settings page.
2. Move the `.pem` file somewhere stable and private, such as
   `~/.config/shipyard/github-apps/shipyard-local.private-key.pem`.
3. Restrict permissions:

```bash
mkdir -p ~/.config/shipyard/github-apps
chmod 700 ~/.config/shipyard ~/.config/shipyard/github-apps
chmod 600 ~/.config/shipyard/github-apps/shipyard-local.private-key.pem
```

4. Install the app on your account.
5. Choose `All repositories` if your goal is the scaled repository-count
   bucket.
6. Save the App ID and Installation ID.

When you add or raise a permission later, saving the App definition does not
update an existing installation automatically. Approve its pending permission
update, then expire or replace cached installation tokens. Tokens minted before
approval retain the old permission set and commonly fail with
`403 Resource not accessible by integration`.

The installation ID is visible in the installation URL:

```text
https://github.com/settings/installations/<installation-id>
```

## Configure Shipyard

Shipyard currently uses GitHub App installation tokens through a command helper.
Put the `[github.auth]` block in one of two places — never in the tracked
project `.shipyard/config.toml`, since it points at private-key paths:

- **Global (recommended for one credential across every repo on the machine):**
  Shipyard's global config dir. Find it with `shipyard paths` (the `global_dir`
  value) — on macOS it is `~/Library/Application Support/shipyard/config.toml`.
  This is what you want when Shipyard inspects many repos from one Mac.
- **Per-repo:** `.shipyard.local/config.toml` in a single repo, if you only want
  the App credential for that one checkout.

Example:

```toml
[github.auth]
source = "command"
token_command = [
  "/Users/you/Code/shipyard/scripts/shipyard-github-app-token",
  "--app-id",
  "123456",
  "--installation-id",
  "987654",
  "--private-key",
  "/Users/you/.config/shipyard/github-apps/shipyard-local.private-key.pem",
  "--repo",
  "{repo_slug}",
]
refresh_skew_seconds = 60
ambient_gh_binary = "/absolute/path/to/gh"
privileged_gh_binary = "/absolute/trusted/path/to/gh"
privileged_git_binary = "/absolute/trusted/path/to/git"
```

Use an absolute helper path. That keeps `shipyard auth export` portable when you
import the config into another repository.

`ambient_gh_binary` is optional and machine-specific. It supplies the direct
native GitHub CLI for the narrowly allowed personal-keyring fallback when an
App is denied a documented low-volume mutation. Shipyard removes ambient token
variables and rejects script/wrapper paths, so do not point it at a `gh` shim
that delegates to the App helper. If omitted, Shipyard scans `PATH` and skips
non-native `gh` wrappers. Update this path after importing the bundle on a
machine with a different GitHub CLI installation.

`privileged_gh_binary` and `privileged_git_binary` are separately required by
the Pulp dependency pin writer. They belong only in machine-global config and
must name trusted absolute native executables; privileged dependency operations
never discover a token recipient through `PATH`. Token-bearing Git runs only
inside a newly initialized isolated repository, excludes inherited/system/global
Git configuration, and releases its credential only to exact HTTPS
`github.com` requests. Privileged children receive a minimal allowlisted
environment rather than inherited loader, proxy, CA, trace, or tool-routing
state. App-authenticated `--delete-branch` also preflights the trusted Git path
before any merge mutation, so an older machine config fails explicitly instead
of silently leaving a branch behind.

You can also use environment variables:

```bash
export SHIPYARD_GITHUB_APP_ID=123456
export SHIPYARD_GITHUB_APP_INSTALLATION_ID=987654
export SHIPYARD_GITHUB_APP_PRIVATE_KEY_PATH="$HOME/.config/shipyard/github-apps/shipyard-local.private-key.pem"
```

Then the config can be shorter:

```toml
[github.auth]
source = "command"
token_command = [
  "/Users/you/Code/shipyard/scripts/shipyard-github-app-token",
  "--repo",
  "{repo_slug}",
]
refresh_skew_seconds = 60
```

## Validate the quota

Run:

```bash
shipyard doctor --rate-limit
```

Expected output should show:

```text
github-auth: ok command helper (github-app-installation)
REST (core): .../12500 remaining
GraphQL: .../12500 remaining
```

If the installation should inspect a runner group, mint a token with the
configured helper and use it for these probes. The helper emits JSON, so this
example extracts the token without assuming a separately installed `ghapp`
wrapper. The App must permit installation on the target organization and must
be installed there; an installation owned only by a personal account cannot
read that organization's runner groups even when it has repository access:

```bash
app_id="123456"
installation_id="987654"
private_key="/absolute/path/to/private-key.pem"
org="Generous-Corp"
group_id="3"
app_token="$(scripts/shipyard-github-app-token \
  --app-id "$app_id" \
  --installation-id "$installation_id" \
  --private-key "$private_key" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')"
GH_TOKEN="$app_token" gh api "orgs/$org/actions/runner-groups/$group_id"
GH_TOKEN="$app_token" gh api "orgs/$org/actions/runner-groups/$group_id/repositories"
GH_TOKEN="$app_token" gh api "orgs/$org/actions/runner-groups/$group_id/runners"
unset app_token app_id installation_id private_key org group_id
```

For `403 Resource not accessible by integration`, compare the App's requested
organization permission with the installation's approved permission, approve
any pending update, and mint a fresh token before changing to a broader
credential. Other 403 responses can be primary or secondary rate limiting;
inspect the response message and `X-RateLimit-Remaining` before changing
permissions.

To export and re-import the non-secret auth config:

```bash
shipyard auth export --output shipyard-auth.toml
shipyard auth import shipyard-auth.toml --scope local
shipyard auth doctor
shipyard doctor --rate-limit
```

The export contains the `[github.auth]` command configuration. It does not
include GitHub tokens, token caches, private keys, Keychain items, or
1Password sessions.

## Additional clients (the same App across several Macs)

One GitHub App installation covers every machine — the private key is the App's
credential, not a per-host secret, so the same `.pem` works on an M1, a Studio,
an M5, etc. Each additional client needs four things:

1. **Shipyard installed** (`shipyard --version`) plus `python3` and `openssl`
   (both stock on macOS — the helper signs the JWT with `openssl` and otherwise
   uses only the Python standard library, so there is nothing to `pip install`).
2. **The token helper** on disk. Either use a local Shipyard checkout's
   `scripts/shipyard-github-app-token`, or fetch the standalone script:

   ```bash
   mkdir -p ~/.config/shipyard/bin
   curl -fsSL https://raw.githubusercontent.com/danielraffel/Shipyard/main/scripts/shipyard-github-app-token \
     -o ~/.config/shipyard/bin/shipyard-github-app-token
   chmod +x ~/.config/shipyard/bin/shipyard-github-app-token
   ```

   The helper first verifies GitHub TLS with Python's configured trust store.
   Only after an actual certificate-verification failure *and* proof that the
   ambient interpreter has no explicit, loaded, or on-disk default CA store does
   it augment that same context with a platform CA file such as
   `/etc/ssl/cert.pem`. Directory-backed, pinned, and private enterprise roots
   are never broadened; certificate verification is never disabled. If neither
   source is usable, it fails closed and asks for `SSL_CERT_FILE` or a repaired
   Python installation.

3. **The same private key**, transferred securely (AirDrop/`scp` — it cannot be
   re-downloaded from GitHub). Store and lock it down exactly as above:

   ```bash
   mkdir -p ~/.config/shipyard/github-apps
   chmod 700 ~/.config/shipyard ~/.config/shipyard/github-apps
   # move shipyard-local.private-key.pem into place, then:
   chmod 600 ~/.config/shipyard/github-apps/shipyard-local.private-key.pem
   ```

4. **The `[github.auth]` block** in this machine's global config dir (find it
   with `shipyard paths`; on macOS `~/Library/Application Support/shipyard/config.toml`),
   with absolute paths to the helper and key on *this* host:

   ```toml
   [github.auth]
   source = "command"
   token_command = [
     "/Users/you/.config/shipyard/bin/shipyard-github-app-token",
     "--app-id", "<your App ID>",
     "--private-key", "/Users/you/.config/shipyard/github-apps/shipyard-local.private-key.pem",
     "--repo", "{repo_slug}",
   ]
   refresh_skew_seconds = 60
   ```

The App must be installed on the account with access to the repos you target, so
the helper's `--repo {repo_slug}` installation lookup resolves. Validate the same
way: `shipyard auth doctor` → `command helper (github-app-installation)` and
`shipyard doctor --rate-limit` → `.../12500 remaining`. `shipyard auth export`
on the first machine plus `shipyard auth import` on the next copies the
*non-secret* config shape, but you still transfer the key and adjust absolute
paths per host.
