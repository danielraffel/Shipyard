# Agent git over HTTPS with a repository-scoped App token

`shipyard auth git-credential` is a git credential helper. When git asks for
`https://github.com/OWNER/REPO`, it mints a GitHub App installation token
**restricted to that one repository** and to the permissions you configure,
and hands it to git. Nothing is stored: every `get` mints a new token, which
GitHub expires after one hour, and git (2.41+) honours the
`password_expiry_utc` the helper returns.

It replaces two broader credentials:

- the owner's SSH key, which made every agent push land as the owner with
  access to every repository the owner can reach;
- an unscoped `ghapp token`, which is valid for every repository the App is
  installed on.

## Machine-global config

Only the machine-global config is read, never a project or local layer, so a
checkout cannot widen what the helper serves.

```toml
[github.auth]
source = "command"
token_command = ["/Users/me/.local/bin/ghapp", "token", "--app-id", "123", "--private-key", "/abs/key.pem", "--repo", "{repo_slug}"]

[git_credential]
# Required. Nothing is served when this is missing or empty.
repositories = ["danielraffel/Shipyard", "danielraffel/tartci", "Generous-Corp/pulp"]
# Optional. Default: ["contents=write"]. Pushing a change under
# .github/workflows/ also needs "workflows=write".
permissions = ["contents=write", "workflows=write"]
```

The helper appends `--scope-repository --permission NAME=LEVEL ...` to the
configured token command. `shipyard-github-app-token` then requests
`{"repositories": [REPO], "permissions": {...}}`. It refuses a response that
names any other repository or any permission beyond the request plus the
implicit `metadata: read`, and it never caches a scoped token.

## git config

```sh
git config --global credential.https://github.com.helper ''
git config --global --add credential.https://github.com.helper '!shipyard auth git-credential'
git config --global credential.https://github.com.useHttpPath true
```

The empty first entry clears inherited helpers (`osxkeychain`,
`gh auth git-credential`). On a refusal (a repository outside the list, a
non-github.com host, or no path because `useHttpPath` is off), the helper
prints `quit=1`, so git neither asks a later helper nor prompts. The push then
fails with the reason on stderr.

## Check it

```sh
printf 'url=https://github.com/danielraffel/Shipyard.git\n\n' | git credential fill >/dev/null && echo minted
git ls-remote https://github.com/danielraffel/agent-skills.git   # outside the list: refused
git push --dry-run https://github.com/danielraffel/Shipyard.git HEAD:refs/heads/probe
```

`git push --dry-run` authenticates for write without changing anything.

## `shipyard pr` pushes

`shipyard pr` and `shipyard ship` push with plain `git push`, so by default
they use whatever credentials the host's git config holds (today the owner's
SSH key). With the machine-global

```toml
[git_credential]
shipyard_push = true
```

the push runs with `GIT_CONFIG_*` entries that rewrite `git@github.com:` to
HTTPS, drop every inherited github.com credential helper, and use
`shipyard auth git-credential`. That holds whatever the host's git config says.
The pushed repository must be in `git_credential.repositories`. Repository
pre-push hooks still run.

## Every ghapp token scoped to its repository

`ghapp` and Shipyard's own `gh` calls mint installation tokens that cover every
repository the App is installed on. With the machine-global

```toml
[github.auth]
scope_to_repository = true
```

each token covers only the repository the call is routed to (`--repo`, or the
checkout's remote), and keeps the App's permissions there.
- `auth helper-argv` adds `--scope-repository` to the arguments it hands
  `ghapp`.
- Shipyard appends it to `token_command`, so its in-memory token cache key
  includes the scope.
- The helper caches repository-scoped and unscoped tokens under separate keys,
  and refuses an entry whose recorded scope differs.

Organisation endpoints such as `orgs/{org}/actions/runners` and
`runner-groups` keep working with a repository-scoped token. A call routed to
one repository that reads or writes a different private repository gets a 404.
Public repositories stay readable.

## Rollback

Remove the helper lines (or restore the previous credential config). Removing
`[git_credential]` also makes the helper refuse everything.
