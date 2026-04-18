# Post-release docs sync

Shipyard closes the "tag landed → CHANGELOG still stale" gap in the release pipeline. Once opted in, every tag push auto-commits a regenerated `CHANGELOG.md` back to `main` with `[skip ci]`. No catch-up PRs, no stale release links.

Two independently usable capabilities ship together:

| | Shape | When to use |
|---|---|---|
| **Generator** | Opinionated CHANGELOG.md + release-notes renderer that walks `v*` tags in reverse semver order | Projects that don't already have a changelog script |
| **Hook runner** | Unopinionated post-tag command runner: run a configured command, watch files, commit with trailers + `[skip ci]`, rebase-retry race loop | Every project — matches the original issue exactly |

A project can mix-and-match: point the hook's `command` at your own script if you don't want shipyard's generator. Or call the generator from your own workflow if you don't want shipyard to own the commit-back side.

## Opt in

```bash
cd your-repo
shipyard changelog init       # scaffolds .shipyard/config.toml + backs up existing CHANGELOG.md
shipyard changelog regenerate # writes CHANGELOG.md from tag graph
shipyard release-bot hook install  # drops .github/workflows/post-tag-sync.yml
git add .shipyard/config.toml CHANGELOG.md .github/workflows/post-tag-sync.yml
shipyard pr                   # merge the opt-in
```

The config block is the opt-in signal. No `[release.changelog]` section in `.shipyard/config.toml` means shipyard never touches the file. No HTML ownership marker, no state file, no `--adopt` flag.

## The `.shipyard/config.toml` block

```toml
[release.changelog]
enabled    = true
path       = "CHANGELOG.md"
repo_url   = "https://github.com/owner/repo"
tag_filter = "v*"                        # strict v1.2.3 shape; excludes plugin-v* and -rc pre-releases
product    = "MyProject"                 # H1-adjacent "All notable changes to {product}…"
skip_commit_patterns = [                 # subjects dropped from entries
  "^chore: bump",
  "^chore\\(release\\):",
  "^bump .*to v?\\d+\\.\\d+\\.\\d+$",
]

[release.post_tag_hook]
enabled              = true
command              = "shipyard changelog regenerate"  # or any custom script
watch                = ["CHANGELOG.md"]
trailers = [
  'Version-Bump: sdk=skip reason="docs-only automated regeneration"',
  'Skill-Update: skip skill=ci reason="no workflow shape change"',
  'Release: skip reason="bot commit; prevent recursive auto-release"',
]
only_for_tag_pattern = "v*"
max_push_attempts    = 5

[release.post_tag_hook.bot_identity]
name  = "shipyard-release-bot"
email = "shipyard-release-bot@users.noreply.github.com"
```

## The installed workflow

`shipyard release-bot hook install` drops `.github/workflows/post-tag-sync.yml`. Shipyard owns this file — re-running install overwrites it in place; removing the hook is a plain `rm`. No YAML surgery on your existing `auto-release.yml`.

The workflow:

1. Fires on `push: tags: ["v*"]` (or your configured pattern).
2. Checks out `main` with full history + tags via `RELEASE_BOT_TOKEN || GITHUB_TOKEN`.
3. Installs shipyard via `curl -fsSL https://generouscorp.com/Shipyard/install.sh | sh`, pinned to a version.
4. Runs `shipyard release-bot hook run --tag "${GITHUB_REF#refs/tags/}"`.

That final command reads `[release.post_tag_hook]`, executes `command`, checks `watch` for diffs, commits with the configured trailers + `[skip ci]`, and pushes back with a rebase-retry loop (up to `max_push_attempts`).

## Best-effort semantics

Docs sync never rolls back a tag. If the hook's command fails, the push races out, or rebase-retry exhausts, the GitHub Release still exists — only the follow-up commit is missing. Re-run `shipyard changelog regenerate` locally and open a normal PR to recover.

## Drift gate for CI

Wire `shipyard changelog check` into your regular PR workflow:

```yaml
- name: Check CHANGELOG.md is in sync with tags
  run: shipyard changelog check
```

Exit codes: `0` in sync, `1` drift, `2` config missing / tag missing.

## Pulp as the reference implementation

The canonical adopter is [pulp](https://github.com/danielraffel/pulp). Its CHANGELOG generator (`tools/scripts/regenerate_changelog.py`, PRs #262 / #265) plus the post-tag block in `auto-release.yml` (PR #294) is exactly what shipyard ships here, parameterized. After the shipyard v0.9.0 release, pulp's migration PR:

1. Adds `.shipyard/config.toml` with `[release.changelog]` set to pulp's values.
2. Verifies byte-identical output: `diff <(shipyard changelog regenerate --stdout) CHANGELOG.md` is empty.
3. Deletes `tools/scripts/regenerate_changelog.py` and the post-tag block in `auto-release.yml`.
4. Runs `shipyard release-bot hook install`.
5. Points `release-cli.yml`'s release-notes step at `shipyard changelog regenerate --release-notes "$TAG"`.

The byte-identical gate is the migration guarantee: pulp sees no output change.

## Troubleshooting

**CHANGELOG didn't update after tag landed.** Check the workflow run under "Actions" in GitHub. Common causes:

- `RELEASE_BOT_TOKEN` missing or expired → run `shipyard doctor --release-chain`.
- `only_for_tag_pattern` doesn't match (e.g., `plugin-v*` tags when pattern is `v*`).
- Generator command exited non-zero → the step logs show the reason.

Recover manually by running `shipyard changelog regenerate` locally and opening a normal PR.

**Drift detected in CI.** Either a human hand-edited the wrong block, or a squash-merge subject matched a skip pattern that shouldn't have. Run `shipyard changelog regenerate` locally, review the diff, and commit.

**Hook loops forever.** Shouldn't happen — the `[skip ci]` subject suppresses re-runs, and the three `*: skip` trailers suppress the version-bump, skill-sync, and auto-release gates. If you see the hook re-triggering itself, the trailers aren't being applied; check `.shipyard/config.toml` for typos.
