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
| Checks | Read-only |
| Commit statuses | Read-only |
| Pull requests | Read-only |
| Metadata | Always available |

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

The installation ID is visible in the installation URL:

```text
https://github.com/settings/installations/<installation-id>
```

## Configure Shipyard

Shipyard currently uses GitHub App installation tokens through a command helper.
Use `.shipyard.local/config.toml` for personal paths so private-key locations do
not land in tracked project config.

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
```

Use an absolute helper path. That keeps `shipyard auth export` portable when you
import the config into another repository.

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
