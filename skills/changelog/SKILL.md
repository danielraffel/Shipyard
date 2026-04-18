---
name: changelog
description: Post-release docs sync — regenerate CHANGELOG.md from git tags, install the post-tag GitHub Actions hook, and diagnose drift. Use when a release just landed and the CHANGELOG didn't update, when a project wants to adopt shipyard-managed release notes, or when CI flags "CHANGELOG.md out of date".
---

# Post-release docs sync

Shipyard-owned changelog regeneration + post-tag commit-back. Opt-in via `[release.changelog]` + `[release.post_tag_hook]` in `.shipyard/config.toml`. Absent sections = no behavior change.

## Quick reference

| Task | Command |
|------|---------|
| Scaffold config + warn about existing CHANGELOG | `shipyard changelog init` |
| Regenerate CHANGELOG.md | `shipyard changelog regenerate` |
| CI drift check | `shipyard changelog check` (exit 1 on drift) |
| Print release notes for one tag | `shipyard changelog regenerate --release-notes v0.9.0` |
| Render to stdout instead of writing | `shipyard changelog regenerate --stdout` |
| Install post-tag workflow | `shipyard release-bot hook install` |
| Run the hook locally (dry tag) | `shipyard release-bot hook run --tag v0.9.0` |

## When to run regen

- **Hook didn't fire.** A tag landed but `CHANGELOG.md` is still stale — either `RELEASE_BOT_TOKEN` is missing, the workflow was uninstalled, or the tag didn't match `only_for_tag_pattern`. Run `shipyard changelog regenerate` locally to catch up, then PR it in.
- **Previewing the next release.** Merge candidate landed in a PR branch. Run regen locally to see what the bullet will look like.
- **CI drift gate.** `shipyard changelog check` in a PR workflow lights up red if a human hand-edited the wrong block and the generator would have produced different output.

## How drift is caught

`shipyard changelog check` rebuilds from scratch and compares to the file on disk. Any difference is "drift." Wire it into CI:

```yaml
- name: Check CHANGELOG.md is in sync with tags
  run: shipyard changelog check
```

Exit codes: `0` clean, `1` drift, `2` config missing / tag missing.

## Config reference — `[release.changelog]`

| Field | Meaning | Default |
|---|---|---|
| `enabled` | Opt-in switch. Absent = no behavior change. | `false` |
| `repo_url` | GitHub repo URL used for PR links + release backlinks. | auto-detect |
| `path` | CHANGELOG path to overwrite. | `CHANGELOG.md` |
| `tag_filter` | Glob of tags to walk. Restricts further to strict `PREFIX + vMAJOR.MINOR.PATCH`. | `v*` |
| `product` | Name in the CHANGELOG header "All notable changes to `{product}`…". | project name |
| `skip_commit_patterns` | Subjects dropped from entries (regex list, case-insensitive). | chore/release bumps |
| `title` | H1 text. | `Changelog` |
| `header_comment` | Full `<!-- … -->` block emitted between intro and first entry. Default omits the block — keeps shipyard-identifying text out of the rendered file. Migrating off another generator? Set this to the previous header verbatim for byte-identity. | `None` |

## Config reference — `[release.post_tag_hook]`

| Field | Meaning | Default |
|---|---|---|
| `enabled` | Opt-in switch. | `false` |
| `command` | Shell command the workflow runs. Swap for a custom script if you don't want shipyard's generator. | `shipyard changelog regenerate` |
| `watch` | Files to stage-and-commit if diffed after `command`. | `["CHANGELOG.md"]` |
| `trailers` | Commit trailers appended to the docs-sync commit. | three `*: skip` trailers |
| `only_for_tag_pattern` | Glob restricting which tags trigger sync. | `v*` |
| `max_push_attempts` | Rebase-retry attempts before giving up. | `5` |
| `bot_identity.name` / `bot_identity.email` | git committer identity for the bot commit. | `shipyard-release-bot` |

## Two capabilities, separately usable

- **Hook only** — point `command` at your own script, keep shipyard's rebase-retry + trailer plumbing.
- **Generator only** — run `shipyard changelog regenerate` from your own workflow if you don't want shipyard to own the committing side.

## Skill-sync gotcha

Anything under `src/shipyard/changelog/**` or `commands/changelog.md` touches this skill's path map. Update this file in the same PR, or pass `--skip-skill-update changelog --skill-reason "..."` to `shipyard pr` (only when the change is genuinely mechanical).
